use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, DefaultCompleter, DefaultHinter, DefaultPrompt, DefaultPromptSegment,
    EditCommand, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Vi, default_emacs_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings, default_vi_visual_keybindings,
};
use serde::Deserialize;

use crate::{
    render::{
        CapturedCodeBlock, ThemeFlavor, build_skin_for_flavor, extract_code_blocks_from_markdown,
        resolve_theme_flavor, str_display_width,
    },
    router::{ImageAttachment, ModelTargets, model_has_baked_effort},
};

pub const HOST_CMD_PASTE_CLIPBOARD: &str = "__paste_clipboard__";
pub const HOST_CMD_COPY_CODE: &str = "__copy_code__";
pub const HOST_CMD_QUOTE_CODE: &str = "__quote_code__";

pub const REPL_COMMANDS: &[&str] = &[
    "/paste",
    "/paste clear",
    "/paste list",
    "/copy",
    "/copy list",
    "/copy all",
    "/c",
    "/c list",
    "/c all",
    "/quote",
    "/quote list",
    "/quote clear",
    "/quote all",
    "/shot",
    "/image",
    "/clear",
    "/resume",
    "/resume all",
    "/new",
    "/compact",
    "/model",
    "/model auto",
    "/model flash-low",
    "/model flash-medium",
    "/model flash-high",
    "/model argon-low",
    "/model argon-medium",
    "/model argon-high",
    "/model carbon-low",
    "/model carbon-medium",
    "/model carbon-high",
    "/effort",
    "/effort auto",
    "/effort low",
    "/effort medium",
    "/effort high",
    "/effort xhigh",
    "/effort max",
    "/diff",
    "/artifact",
    "/tui",
    "/status",
    "/preset spectrum",
    "/preset no-carbon",
    "/preset claude",
    "/theme auto",
    "/theme light",
    "/theme dark",
    "/vi",
    "/emacs",
    "/help",
    "/exit",
    "/quit",
];

pub const AGY_NATIVE_SUBCOMMANDS: &[&str] = &[
    "acp",
    "agent",
    "agents",
    "changelog",
    "help",
    "install",
    "mcp",
    "mic-serve",
    "models",
    "plugin",
    "plugins",
    "remote-control",
    "update",
];

pub const SUPPORTED_IMAGE_MIMES: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

pub const DEFAULT_IMAGE_ANALYSIS_PROMPT: &str =
    "첨부한 스크린샷 이미지를 확인하고 핵심 내용을 분석해 줘.";

#[derive(Debug, Clone, Deserialize)]
pub struct ConversationRow {
    pub conversation_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub step_count: i64,
    #[serde(default)]
    pub last_modified_time: String,
    #[serde(default)]
    pub workspace_uris: String,
}

#[derive(Debug, Deserialize)]
struct TranscriptStepRecord {
    #[serde(default, rename = "type")]
    step_type: String,
    #[serde(default)]
    content: String,
}

pub enum ClipboardPayload {
    Image(ImageAttachment),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct PendingQuote {
    pub label: String,
    pub content: String,
}

/// `agy-route`와 `agy-acp`가 공통으로 사용하는 REPL 세션 상태
pub struct SharedReplState {
    pub app_tag: &'static str,
    pub history_filename: &'static str,
    pub targets: ModelTargets,
    pub active_flavor: ThemeFlavor,
    pub detected_luma: Option<f32>,
    pub current_edit_mode: String,
    pub active_conv_id: Option<String>,
    pub use_continue_flag: bool,
    pub pinned_model: Option<String>,
    pub pinned_effort: Option<String>,
    pub last_model: String,
    pub last_effort: Option<String>,
    pub turn_idx: usize,
    pub pending_images: Vec<ImageAttachment>,
    pub pending_quotes: Vec<PendingQuote>,
    pub paste_seq: usize,
    pub last_code_blocks: Vec<CapturedCodeBlock>,
    pub last_raw_response: String,
}

impl SharedReplState {
    pub fn new(
        app_tag: &'static str,
        history_filename: &'static str,
        targets: ModelTargets,
        active_flavor: ThemeFlavor,
        detected_luma: Option<f32>,
        edit_mode: String,
        active_conv_id: Option<String>,
        use_continue_flag: bool,
    ) -> Self {
        let initial_turns = active_conv_id
            .as_deref()
            .map(count_user_turns_in_transcript)
            .unwrap_or(0);
        // 기본 주력 모델을 carbon-medium으로 고정 (캐시 100% 보존 및 빠른 반응, /model auto로 언제든 자동 라우팅 전환 가능)
        let default_main_model = targets.variant("carbon", "medium");
        let mut s = Self {
            app_tag,
            history_filename,
            targets,
            active_flavor,
            detected_luma,
            current_edit_mode: edit_mode,
            active_conv_id: active_conv_id.clone(),
            use_continue_flag,
            pinned_model: Some(default_main_model.clone()),
            pinned_effort: None,
            last_model: default_main_model,
            last_effort: None,
            turn_idx: initial_turns,
            pending_images: Vec::new(),
            pending_quotes: Vec::new(),
            paste_seq: 0,
            last_code_blocks: Vec::new(),
            last_raw_response: String::new(),
        };
        if let Some(cid) = active_conv_id.as_deref() {
            s.hydrate_from_transcript(cid);
        }
        // 이전 세션에서 남아 있던 외부 인용 큐 파일 초기화
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let _ = fs::remove_file(Path::new(&runtime_dir).join("agy-quote-queue.txt"));
        s
    }

    /// 트랜스크립트의 가장 최근 어시스턴트 응답에서 전문과 코드 블록을 복원합니다.
    pub fn hydrate_from_transcript(&mut self, conv_id: &str) {
        let (raw_resp, blocks) = extract_turn_context_and_code_blocks_from_transcript(conv_id);
        if !raw_resp.is_empty() || !blocks.is_empty() {
            self.record_turn_output(blocks, raw_resp);
        }
    }

    /// 터미널에서 `[❝ 인용]` 링크를 클릭해 `$XDG_RUNTIME_DIR/agy-quote-queue.txt`에 적재된 항목을 동기화합니다.
    pub fn sync_external_quotes(&mut self) {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let queue_path = Path::new(&runtime_dir).join("agy-quote-queue.txt");
        let Ok(raw) = fs::read_to_string(&queue_path) else {
            return;
        };
        let _ = fs::remove_file(&queue_path);

        for line in raw.lines() {
            let clip_id = line.trim();
            if clip_id.is_empty() {
                continue;
            }
            if let Some(block) = self.last_code_blocks.iter().find(|b| b.id == clip_id) {
                let lang_label = if block.lang.is_empty() {
                    "code"
                } else {
                    &block.lang
                };
                let lines = block.code.lines().count();
                let label = format!("#{} {} ({} lines)", block.index, lang_label, lines);
                let content = format!("```{}\n{}\n```", block.lang, block.code.trim_end());
                self.pending_quotes.push(PendingQuote { label, content });
                continue;
            }
            let clip_file = Path::new(&runtime_dir).join(format!("agy-clip-{clip_id}.txt"));
            if let Ok(text) = fs::read_to_string(&clip_file)
                && !text.trim().is_empty()
            {
                let lines = text.lines().count();
                let label = if clip_id.starts_with("turn_") {
                    format!("직전 답변 전문 ({lines} lines)")
                } else {
                    format!("클릭 인용 ({lines} lines)")
                };
                let quoted = text
                    .lines()
                    .map(|l| format!("> {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.pending_quotes.push(PendingQuote {
                    label,
                    content: quoted,
                });
            }
        }
    }

    pub fn format_prompt_left(&self) -> String {
        let session_badge = self
            .active_conv_id
            .as_deref()
            .map(|id| &id[..id.len().min(8)])
            .unwrap_or(if self.use_continue_flag {
                "recent"
            } else {
                "new"
            });
        let mode_badge = match (&self.pinned_model, &self.pinned_effort) {
            (Some(m), Some(e)) => format!(" [{m}:{e}]"),
            (Some(m), None) => format!(" [{m}]"),
            (None, Some(e)) => format!(" [auto:{e}]"),
            (None, None) => String::new(),
        };
        let code_badge = match self.last_code_blocks.len() {
            0 => String::new(),
            1 => " [📋#1]".to_string(),
            n => format!(" [📋#1..{n}]"),
        };
        let quote_badge = if self.pending_quotes.is_empty() {
            String::new()
        } else {
            format!(" [💬x{}]", self.pending_quotes.len())
        };
        let img_badge = if self.pending_images.is_empty() {
            String::new()
        } else {
            format!(" [📷x{}]", self.pending_images.len())
        };
        format!(
            "[{}:{session_badge} #{}{mode_badge}{code_badge}{quote_badge}{img_badge}] ",
            self.app_tag,
            self.turn_idx + 1
        )
    }

    pub fn build_reedline_prompt(&mut self) -> (String, DefaultPrompt) {
        self.sync_external_quotes();
        let left_seg = self.format_prompt_left();
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(left_seg.clone()),
            DefaultPromptSegment::Empty,
        );
        (left_seg, prompt)
    }

    pub fn record_turn_output(&mut self, blocks: Vec<CapturedCodeBlock>, raw_resp: String) {
        if !blocks.is_empty() {
            self.last_code_blocks = blocks;
        }
        if !raw_resp.is_empty() {
            let runtime_dir =
                std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
            let turn_id = format!(
                "turn_{:x}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            );
            let turn_path = Path::new(&runtime_dir).join(format!("agy-clip-{turn_id}.txt"));
            let _ = fs::write(&turn_path, &raw_resp);
            let _ = fs::write(
                Path::new(&runtime_dir).join("agy-clip-latest-turn.id"),
                &turn_id,
            );
            self.last_raw_response = raw_resp;
        }
    }
}

/// 공통 슬래시 명령어 분기 결과
pub enum CommonSlashAction {
    NotSlash,
    Handled,
    Exit,
    NewSession { clear_screen: bool },
    Resume { arg: String },
    RunPrompt(String),
    SwitchToTui,
    Status,
}

pub enum ReadLineTurnOutcome {
    Submit { raw_line: String, left_prompt: String },
    ContinueLoop { skip_newline: bool },
    Exit,
}

/// 표준 XDG 설정(`~/.config/agy-decision-lab/config.env`) 및 런타임 환경변수를 로드합니다.
pub fn load_lab_dotenv() {
    let _ = crate::runtime_config::load_runtime_env();
}

/// 첫 번째 위치 인자가 `agy` 고유 서브커맨드인 경우 즉시 `exec`로 위임합니다.
pub fn maybe_exec_native_subcommand(agy_bin: &str) -> Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = raw_args.first()
        && !first.starts_with('-')
        && AGY_NATIVE_SUBCOMMANDS.contains(&first.as_str())
    {
        let mut cmd = Command::new(agy_bin);
        cmd.env_remove("GEMINI_API_KEY");
        cmd.args(&raw_args);
        let err = cmd.exec();
        bail!("'{}' 서브커맨드 실행 실패: {}", agy_bin, err);
    }
    Ok(())
}

pub fn resolve_targets(preset: Option<&str>) -> ModelTargets {
    match preset.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("no-carbon" | "argon") => ModelTargets {
            hydrogen_base: "gemini-4-hydrogen".to_string(),
            argon_base: "gemini-4-argon".to_string(),
            carbon_base: "gemini-4-argon".to_string(),
        },
        Some("claude") => ModelTargets {
            hydrogen_base: "gemini-4-hydrogen".to_string(),
            argon_base: "claude-sonnet-5-5".to_string(),
            carbon_base: "claude-opus-5-5".to_string(),
        },
        Some("spectrum" | "default") | None => ModelTargets::from_env(),
        _ => ModelTargets::from_env(),
    }
}

pub fn apply_effort_override(model: &str, effort_override: Option<&str>) -> (String, Option<String>) {
    let Some(eff) = effort_override else {
        let cli_eff = (!model_has_baked_effort(model)).then(|| "medium".to_string());
        return (model.to_string(), cli_eff);
    };

    let base = model
        .strip_suffix("-xhigh")
        .or_else(|| model.strip_suffix("-low"))
        .or_else(|| model.strip_suffix("-medium"))
        .or_else(|| model.strip_suffix("-high"))
        .unwrap_or(model);

    if matches!(eff, "low" | "medium" | "high") && model_has_baked_effort(model) {
        return (format!("{base}-{eff}"), None);
    }

    // xhigh 지정 시 Hydrogen처럼 네이티브 -xhigh 슬러그가 있으면 플래그 없이 직접 결합하고, 아니면 -high + --effort xhigh 전달
    if eff == "xhigh" && model_has_baked_effort(model) {
        if ModelTargets::supports_xhigh_slug(base) {
            return (format!("{base}-xhigh"), None);
        }
        return (format!("{base}-high"), Some("xhigh".to_string()));
    }

    // max 등 상위 effort가 지정된 경우 xhigh 지원 패밀리는 -xhigh, 그 외는 -high 모델 + --effort 전달
    if eff == "max" && model_has_baked_effort(model) {
        let top = if ModelTargets::supports_xhigh_slug(base) {
            "xhigh"
        } else {
            "high"
        };
        return (format!("{base}-{top}"), Some("max".to_string()));
    }

    (model.to_string(), Some(eff.to_string()))
}

pub fn reset_terminal_mouse_and_modes() {
    let mut err = io::stderr();
    let _ = err.write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1015l");
    let _ = err.flush();
}

pub fn clear_terminal_screen() {
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[2J\x1b[3J\x1b[H");
    let _ = out.flush();
}

pub fn current_terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// 사용자가 타이핑했던 원시 프롬프트 행이 터미널에서 차지한 줄 수를 계산합니다.
pub fn calculate_input_line_rows(prompt_str: &str, input_str: &str, term_width: usize) -> usize {
    let term_width = term_width.max(10);
    let prompt_w = str_display_width(prompt_str) + 2;
    let lines: Vec<&str> = input_str.split('\n').collect();
    if lines.is_empty() {
        return 1;
    }

    let mut total_rows = 0;
    for (i, line) in lines.iter().enumerate() {
        let line_w = str_display_width(line);
        // 멀티라인 2번째 줄부터는 reedline 기본 연속 프롬프트("::: ", 4칸) 폭을 반영
        let row_w = if i == 0 { prompt_w + line_w } else { 4 + line_w };
        let rows = if row_w == 0 {
            1
        } else {
            row_w.div_ceil(term_width)
        };
        total_rows += rows.max(1);
    }
    total_rows
}

/// 터미널에서 커서를 위로 올리며 지정한 줄 수만큼 지웁니다 (In-place 버블 교체용).
pub fn erase_previous_terminal_lines(num_lines: usize) {
    if num_lines == 0 {
        return;
    }
    let mut out = io::stdout();
    for _ in 0..num_lines {
        let _ = out.write_all(b"\x1b[1A\x1b[2K");
    }
    let _ = out.write_all(b"\r");
    let _ = out.flush();
}

/// 타이핑했던 원시 입력 줄을 즉시 지워 그 자리에 둥근 말풍선이 덮어써지도록 합니다.
pub fn erase_prompt_line_in_place(prompt_str: &str, input_str: &str) {
    let term_width = current_terminal_width();
    let rows = calculate_input_line_rows(prompt_str, input_str, term_width);
    erase_previous_terminal_lines(rows);
}

pub fn build_emacs_keybindings() -> reedline::Keybindings {
    let tab_event = ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu("completion_menu".to_string()),
        ReedlineEvent::MenuNext,
    ]);
    let paste_event = ReedlineEvent::ExecuteHostCommand(HOST_CMD_PASTE_CLIPBOARD.to_string());
    let copy_event = ReedlineEvent::ExecuteHostCommand(HOST_CMD_COPY_CODE.to_string());
    let quote_event = ReedlineEvent::ExecuteHostCommand(HOST_CMD_QUOTE_CODE.to_string());
    let insert_newline_event = ReedlineEvent::Edit(vec![EditCommand::InsertNewline]);

    let mut emacs_kb = default_emacs_keybindings();
    emacs_kb.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_event);
    emacs_kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('v'),
        paste_event.clone(),
    );
    emacs_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('v'), paste_event);
    emacs_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('c'), copy_event.clone());
    emacs_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('y'), copy_event);
    emacs_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('q'), quote_event);
    // Ctrl+J, Alt+Enter, Shift+Enter -> 전송(Enter) 대신 줄바꿈(개행) 삽입으로 오버라이드
    emacs_kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('j'),
        insert_newline_event.clone(),
    );
    emacs_kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        insert_newline_event.clone(),
    );
    emacs_kb.add_binding(KeyModifiers::SHIFT, KeyCode::Enter, insert_newline_event);
    emacs_kb
}

pub fn build_vi_keybindings() -> (reedline::Keybindings, reedline::Keybindings, reedline::Keybindings) {
    let tab_event = ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu("completion_menu".to_string()),
        ReedlineEvent::MenuNext,
    ]);
    let paste_event = ReedlineEvent::ExecuteHostCommand(HOST_CMD_PASTE_CLIPBOARD.to_string());
    let copy_event = ReedlineEvent::ExecuteHostCommand(HOST_CMD_COPY_CODE.to_string());
    let quote_event = ReedlineEvent::ExecuteHostCommand(HOST_CMD_QUOTE_CODE.to_string());
    let insert_newline_event = ReedlineEvent::Edit(vec![EditCommand::InsertNewline]);

    let mut insert_kb = default_vi_insert_keybindings();
    insert_kb.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_event);
    insert_kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('v'),
        paste_event.clone(),
    );
    insert_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('v'), paste_event.clone());
    insert_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('c'), copy_event.clone());
    insert_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('y'), copy_event.clone());
    insert_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('q'), quote_event.clone());
    insert_kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('j'),
        insert_newline_event.clone(),
    );
    insert_kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        insert_newline_event.clone(),
    );
    insert_kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Enter,
        insert_newline_event.clone(),
    );

    let mut normal_kb = default_vi_normal_keybindings();
    normal_kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('v'),
        paste_event.clone(),
    );
    normal_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('v'), paste_event);
    normal_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('c'), copy_event.clone());
    normal_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('y'), copy_event);
    normal_kb.add_binding(KeyModifiers::ALT, KeyCode::Char('q'), quote_event);
    normal_kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('j'),
        insert_newline_event.clone(),
    );
    normal_kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        insert_newline_event.clone(),
    );
    normal_kb.add_binding(KeyModifiers::SHIFT, KeyCode::Enter, insert_newline_event);

    let visual_kb = default_vi_visual_keybindings();
    (insert_kb, normal_kb, visual_kb)
}

pub fn build_reedline_editor(
    edit_mode_name: &str,
    flavor: ThemeFlavor,
    history_filename: &str,
) -> Result<Reedline> {
    let hist_path = state_base_dir().join(history_filename);
    if let Some(parent) = hist_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let hint_color = match flavor {
        ThemeFlavor::Light => Color::Fixed(244),
        ThemeFlavor::Dark => Color::Fixed(243),
    };

    let commands: Vec<String> = REPL_COMMANDS.iter().map(|s| (*s).to_string()).collect();
    let completer = Box::new(DefaultCompleter::new_with_wordlen(commands, 1));
    let completion_menu = Box::new(ColumnarMenu::default().with_name("completion_menu"));
    let hinter = Box::new(DefaultHinter::default().with_style(Style::new().italic().fg(hint_color)));

    let mut editor = Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_hinter(hinter);

    // 모던 터미널(Ghostty, Kitty, WezTerm, Foot 등)에서 Shift+Enter 등을 정확히 분리 감지
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        editor = editor.use_kitty_keyboard_enhancement(true);
    }

    if let Ok(history) = FileBackedHistory::with_file(500, hist_path) {
        editor = editor.with_history(Box::new(history));
    }

    if edit_mode_name.eq_ignore_ascii_case("vi") || edit_mode_name.eq_ignore_ascii_case("vim") {
        let (insert_kb, normal_kb, visual_kb) = build_vi_keybindings();
        editor = editor.with_edit_mode(Box::new(Vi::new(insert_kb, normal_kb, visual_kb)));
    } else {
        let emacs_kb = build_emacs_keybindings();
        editor = editor.with_edit_mode(Box::new(Emacs::new(emacs_kb)));
    }

    Ok(editor)
}

pub fn print_repl_banner(state: &SharedReplState, subtitle_prefix: &str) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let theme_note = match state.detected_luma {
        Some(l) => format!("{} (auto luma={l:.2})", state.active_flavor.as_str()),
        None => state.active_flavor.as_str().to_string(),
    };
    let routing_label = match &state.pinned_model {
        Some(m) => format!("주력 모델=\x1b[1m{m}\x1b[0m (자동 전환: `/model auto`)"),
        None => format!(
            "자동 라우터={}-* / {}-* / {}-*",
            state.targets.hydrogen_base, state.targets.argon_base, state.targets.carbon_base
        ),
    };

    eprintln!(
        "{accent}\x1b[1m[{}]\x1b[0m {subtitle_prefix} ({routing_label}, theme={theme_note}, mode={})",
        state.app_tag, state.current_edit_mode
    );
    eprintln!(
        "{muted}  단축키: Ctrl+J·Alt+Enter 줄바꿈 | Ctrl+V 스샷첨부 | Alt+C 복사 | Alt+Q 인용 | /model auto | /help\x1b[0m"
    );
}

/// `reedline`에서 1줄 입력 및 호스트 단축키(`Ctrl+V`, `Alt+C`, `Alt+Q`, `Ctrl+C`, `Ctrl+D`)를 공통 처리합니다.
pub fn read_repl_line(
    editor: &mut Reedline,
    state: &mut SharedReplState,
) -> ReadLineTurnOutcome {
    let (left_seg, prompt) = state.build_reedline_prompt();
    match editor.read_line(&prompt) {
        Ok(Signal::Success(buffer)) => ReadLineTurnOutcome::Submit {
            raw_line: buffer,
            left_prompt: left_seg,
        },
        Ok(Signal::HostCommand(cmd)) if cmd == HOST_CMD_PASTE_CLIPBOARD => {
            attach_clipboard_image_interactive(state, Some(editor));
            ReadLineTurnOutcome::ContinueLoop { skip_newline: true }
        }
        Ok(Signal::HostCommand(cmd)) if cmd == HOST_CMD_COPY_CODE => {
            copy_last_code_or_response(state, None);
            ReadLineTurnOutcome::ContinueLoop { skip_newline: true }
        }
        Ok(Signal::HostCommand(cmd)) if cmd == HOST_CMD_QUOTE_CODE => {
            handle_quote_command("", state, editor);
            ReadLineTurnOutcome::ContinueLoop { skip_newline: true }
        }
        Ok(Signal::CtrlC(_)) => {
            if !state.pending_images.is_empty() || !state.pending_quotes.is_empty() {
                let img_cnt = state.pending_images.len();
                let quote_cnt = state.pending_quotes.len();
                state.pending_images.clear();
                state.pending_quotes.clear();
                eprintln!(
                    "{}[{}] 대기 중이던 첨부(이미지 {}개, 인용 {}개)를 모두 취소했습니다.\x1b[0m",
                    state.active_flavor.muted_ansi(),
                    state.app_tag,
                    img_cnt,
                    quote_cnt
                );
            }
            ReadLineTurnOutcome::ContinueLoop {
                skip_newline: false,
            }
        }
        Ok(Signal::CtrlD) => ReadLineTurnOutcome::Exit,
        Ok(_) => ReadLineTurnOutcome::ContinueLoop {
            skip_newline: false,
        },
        Err(err) => {
            eprintln!(
                "{}[{}] 입력 오류: {err}\x1b[0m",
                state.active_flavor.warn_ansi(),
                state.app_tag
            );
            ReadLineTurnOutcome::Exit
        }
    }
}

/// 양쪽 REPL에서 완전히 동일하게 동작하는 슬래시 명령어를 일괄 처리합니다.
pub fn dispatch_common_slash_command(
    trimmed: &str,
    state: &mut SharedReplState,
    line_editor: &mut Reedline,
) -> Result<CommonSlashAction> {
    if !trimmed.starts_with('/') {
        return Ok(CommonSlashAction::NotSlash);
    }

    let (cmd_name, arg_rest) = match trimmed.split_once(char::is_whitespace) {
        Some((c, rest)) => (c, rest.trim()),
        None => (trimmed, ""),
    };

    let accent = state.active_flavor.accent_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    match cmd_name {
        "/exit" | "/quit" | "/q" => Ok(CommonSlashAction::Exit),

        "/help" | "/?" => {
            print_help_menu(state.active_flavor);
            Ok(CommonSlashAction::Handled)
        }

        "/paste" | "/p" | "/shot" | "/screenshot" | "/image" | "/img" => {
            if let Some(prompt_to_run) = handle_paste_command(arg_rest, state) {
                return Ok(CommonSlashAction::RunPrompt(prompt_to_run));
            }
            Ok(CommonSlashAction::Handled)
        }

        "/copy" | "/c" => {
            copy_last_code_or_response(
                state,
                if arg_rest.is_empty() {
                    None
                } else {
                    Some(arg_rest)
                },
            );
            Ok(CommonSlashAction::Handled)
        }

        "/quote" => {
            handle_quote_command(arg_rest, state, line_editor);
            Ok(CommonSlashAction::Handled)
        }

        "/clear" => Ok(CommonSlashAction::NewSession { clear_screen: true }),
        "/new" => Ok(CommonSlashAction::NewSession {
            clear_screen: false,
        }),

        "/resume" | "/r" | "/continue" | "/cs" => Ok(CommonSlashAction::Resume {
            arg: arg_rest.to_string(),
        }),

        "/model" | "/m" => {
            handle_model_command(arg_rest, state);
            Ok(CommonSlashAction::Handled)
        }

        "/effort" | "/e" => {
            handle_effort_command(arg_rest, state);
            Ok(CommonSlashAction::Handled)
        }

        "/compact" => {
            if state.active_conv_id.is_none() && !state.use_continue_flag && state.turn_idx == 0 {
                eprintln!("{warn}[{tag}]\x1b[0m 아직 압축할 활성 대화 세션이 없습니다.");
                return Ok(CommonSlashAction::Handled);
            }
            let compact_prompt = if arg_rest.is_empty() {
                "/compact".to_string()
            } else {
                format!("/compact {arg_rest}")
            };
            eprintln!("{accent}[{tag}]\x1b[0m 현재 대화 세션을 압축(/compact)합니다...");
            Ok(CommonSlashAction::RunPrompt(compact_prompt))
        }

        "/diff" => {
            run_git_diff_summary(tag, state.active_flavor);
            Ok(CommonSlashAction::Handled)
        }

        "/artifact" | "/artifacts" | "/open" => {
            handle_artifact_command(arg_rest, state);
            Ok(CommonSlashAction::Handled)
        }

        "/preset" => {
            if arg_rest.is_empty() {
                eprintln!(
                    "사용 가능한 프리셋: spectrum (Hydrogen+Argon+Carbon 전체 순환), no-carbon (Hydrogen+Argon만 사용), claude (Hydrogen+Sonnet+Opus)"
                );
            } else {
                state.targets = resolve_targets(Some(arg_rest));
                eprintln!(
                    "{accent}[{tag}]\x1b[0m 프리셋 변경됨 ({arg_rest}): {}-* / {}-* / {}-*",
                    state.targets.hydrogen_base, state.targets.argon_base, state.targets.carbon_base
                );
            }
            Ok(CommonSlashAction::Handled)
        }

        "/theme" => {
            let pref = if arg_rest.is_empty() {
                "auto"
            } else {
                arg_rest
            };
            let (new_flavor, luma_opt) = resolve_theme_flavor(pref);
            state.active_flavor = new_flavor;
            state.detected_luma = luma_opt;
            *line_editor = build_reedline_editor(
                &state.current_edit_mode,
                state.active_flavor,
                state.history_filename,
            )?;
            match luma_opt {
                Some(l) => eprintln!(
                    "{}[{tag}]\x1b[0m 테마 자동 재감지됨: {} (배경 luma={l:.2})",
                    state.active_flavor.accent_ansi(),
                    state.active_flavor.as_str()
                ),
                None => eprintln!(
                    "{}[{tag}]\x1b[0m 테마 설정됨: {}",
                    state.active_flavor.accent_ansi(),
                    state.active_flavor.as_str()
                ),
            }
            Ok(CommonSlashAction::Handled)
        }

        "/vi" | "/vim" => {
            state.current_edit_mode = "vi".to_string();
            *line_editor = build_reedline_editor(
                &state.current_edit_mode,
                state.active_flavor,
                state.history_filename,
            )?;
            eprintln!("{accent}[{tag}]\x1b[0m 입력 모드가 Vi 키맵으로 전환되었습니다.");
            Ok(CommonSlashAction::Handled)
        }

        "/emacs" => {
            state.current_edit_mode = "emacs".to_string();
            *line_editor = build_reedline_editor(
                &state.current_edit_mode,
                state.active_flavor,
                state.history_filename,
            )?;
            eprintln!("{accent}[{tag}]\x1b[0m 입력 모드가 기본(Emacs) 키맵으로 전환되었습니다.");
            Ok(CommonSlashAction::Handled)
        }

        "/status" => Ok(CommonSlashAction::Status),
        "/tui" => Ok(CommonSlashAction::SwitchToTui),

        _ => Ok(CommonSlashAction::NotSlash),
    }
}

pub fn print_help_menu(flavor: ThemeFlavor) {
    let accent = flavor.accent_ansi();
    eprintln!("{accent}  [입력 및 줄바꿈]\x1b[0m");
    eprintln!("  Enter               프롬프트 즉시 전송");
    eprintln!("  Ctrl+J / Alt+Enter  전송하지 않고 줄바꿈(개행) 삽입 (모던 터미널은 Shift+Enter 지원)");
    eprintln!("  Ctrl+C              대기 중인 첨부 취소 또는 실행 중인 현재 턴 안전 중단");
    eprintln!();
    eprintln!("{accent}  [스크린샷 및 이미지 첨부]\x1b[0m");
    eprintln!("  Ctrl+V / Alt+V      입력 줄 어디서든 클립보드의 스크린샷(또는 텍스트) 즉시 첨부");
    eprintln!("  /paste [질문|경로]   클립보드 스크린샷 또는 로컬 이미지 파일 첨부 (list | clear 지원)");
    eprintln!("  @/경로/이미지.png    프롬프트 본문 내 로컬 이미지 경로 자동 감지 및 첨부");
    eprintln!();
    eprintln!("{accent}  [코드 복사 및 인용]\x1b[0m");
    eprintln!("  Alt+C / Alt+Y       가장 최근 코드 블록을 클립보드에 즉시 복사");
    eprintln!("  Alt+Q               마우스 드래그 선택 영역 또는 최근 코드 블록을 인용 대기열에 추가");
    eprintln!("  /copy, /c [인자]    코드 복사 (/c 1, /c 1:2-5 줄범위, /c all 전체, /c list 상태조회)");
    eprintln!("  /quote [인자]       인용 예약 (/quote 1, /quote 1:2-5, /quote all, /quote list, /quote clear)");
    eprintln!("  @#1, @#2            프롬프트 내에 작성 시 해당 번호의 코드 블록으로 자동 확장 인용");
    eprintln!();
    eprintln!("{accent}  [세션 및 화면 제어]\x1b[0m");
    eprintln!("  /clear              화면 지우기 및 새 대화 세션으로 초기화");
    eprintln!("  /new                화면을 유지한 채 새 대화 세션으로 초기화");
    eprintln!("  /resume [인자]       최근 대화 목록 조회 및 이어하기 (번호, ID 프리픽스, all, 검색어)");
    eprintln!("  /compact [지시]      현재 대화 히스토리를 요약·압축하여 컨텍스트 여유 확보");
    eprintln!("  /tui                현재 대화를 그대로 순정 agy 풀스크린 TUI로 전환");
    eprintln!();
    eprintln!("{accent}  [모델 및 라우팅 제어]\x1b[0m");
    eprintln!(
        "  /model [모델|auto]   특정 모델로 수동 고정하거나 auto(10단계 동적 라우팅)로 복귀"
    );
    eprintln!("  /effort [수준|auto]  추론 깊이(low|medium|high|xhigh|max) 고정 또는 auto 복귀");
    eprintln!("  /preset <이름>       spectrum (기본) | no-carbon | claude 패밀리 프리셋 변경");
    eprintln!();
    eprintln!("{accent}  [도구 및 환경]\x1b[0m");
    eprintln!("  /diff               현재 Git 작업 트리의 변경 상태 및 diff 요약 확인");
    eprintln!("  /artifact [번호]     현재 세션에서 생성된 아티팩트(.md 등) 목록 및 본문 열람");
    eprintln!("  /theme <모드>        auto (OSC 11 배경 밝기 재감지) | light | dark");
    eprintln!("  /vi , /emacs        입력 줄 편집 키맵을 Vi 또는 Emacs 모드로 즉시 변경");
    eprintln!("  /status             세션, 턴, 모델, 캡처된 코드 블록 및 인용/이미지 대기 상태 확인");
    eprintln!("  /exit (또는 Ctrl+D)  REPL 종료");
}

pub fn print_common_status(state: &SharedReplState, active_server_model: Option<&str>) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let routing_mode = state
        .pinned_model
        .as_deref()
        .map(|m| format!("고정({m})"))
        .unwrap_or_else(|| "자동(10단계 스펙트럼)".to_string());
    let effort_mode = state.pinned_effort.as_deref().unwrap_or("자동");
    let model_label = active_server_model.unwrap_or(&state.last_model);

    eprintln!(
        "{accent}[{}]\x1b[0m 세션: {} | 턴: {} | 라우팅: {} | effort: {} | 활성/직전 모델: {}",
        state.app_tag,
        state.active_conv_id.as_deref().unwrap_or("(미생성)"),
        state.turn_idx,
        routing_mode,
        effort_mode,
        model_label,
    );
    eprintln!(
        "{muted}  상태: 캡처된 코드블록 {}개 (직전응답 {}자) | 인용대기 💬{}개 | 이미지대기 📷{}개\x1b[0m",
        state.last_code_blocks.len(),
        state.last_raw_response.len(),
        state.pending_quotes.len(),
        state.pending_images.len(),
    );
    if !state.last_code_blocks.is_empty() {
        for b in &state.last_code_blocks {
            let lang = if b.lang.is_empty() { "code" } else { &b.lang };
            let preview = collapse_whitespace(b.code.lines().next().unwrap_or(""), 52);
            eprintln!(
                "  {accent}📋 #{}\x1b[0m {lang} ({} lines) {muted}— {preview}\x1b[0m",
                b.index,
                b.code.lines().count()
            );
        }
    }
    if !state.pending_quotes.is_empty() {
        for (i, q) in state.pending_quotes.iter().enumerate() {
            eprintln!("  {accent}💬 인용대기 #{}\x1b[0m {}", i + 1, q.label);
        }
    }
    eprintln!(
        "{muted}  테마: {} | 패밀리: {}/{}/{} | 편집모드: {}\x1b[0m",
        state.active_flavor.as_str(),
        state.targets.hydrogen_base,
        state.targets.argon_base,
        state.targets.carbon_base,
        state.current_edit_mode
    );
}

/// Ctrl+V / Alt+V 입력 시 클립보드에 이미지가 있으면 첨부 목록에 추가하고, 일반 텍스트면 에디터에 삽입합니다.
pub fn attach_clipboard_image_interactive(
    state: &mut SharedReplState,
    line_editor: Option<&mut Reedline>,
) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    state.paste_seq += 1;
    match capture_clipboard_payload(state.paste_seq) {
        Ok(ClipboardPayload::Image(img)) => {
            let kb = (img.byte_len as f64 / 1024.0).max(0.1);
            let path_str = img.path.display().to_string();
            state.pending_images.push(img.clone());
            eprintln!(
                "\r\x1b[2K{accent}[{tag}]\x1b[0m 📷 클립보드 스크린샷 첨부됨 (\x1b[1m#{}\x1b[0m, {}, {:.1} KB): {muted}{path_str}\x1b[0m",
                state.pending_images.len(),
                img.mime_type,
                kb
            );
        }
        Ok(ClipboardPayload::Text(text)) => {
            if let Some(editor) = line_editor {
                editor.run_edit_commands(&[EditCommand::InsertString(text)]);
            } else {
                eprintln!(
                    "{warn}[{tag}]\x1b[0m 클립보드에 이미지가 없습니다 (일반 텍스트만 존재합니다)."
                );
            }
        }
        Err(err) => {
            eprintln!("\r\x1b[2K{warn}[{tag}]\x1b[0m 클립보드 붙여넣기 실패: {err:#}");
        }
    }
}

/// `/paste [인자]` 슬래시 명령어를 처리합니다 (`Some(prompt)` 반환 시 즉시 턴 실행).
pub fn handle_paste_command(arg: &str, state: &mut SharedReplState) -> Option<String> {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    let trimmed = arg.trim();
    if trimmed.eq_ignore_ascii_case("clear")
        || trimmed.eq_ignore_ascii_case("drop")
        || trimmed.eq_ignore_ascii_case("rm")
    {
        let n = state.pending_images.len();
        state.pending_images.clear();
        eprintln!("{accent}[{tag}]\x1b[0m 첨부 대기 중이던 이미지 {n}개를 모두 비웠습니다.");
        return None;
    }

    if trimmed.eq_ignore_ascii_case("list") || trimmed.eq_ignore_ascii_case("ls") {
        if state.pending_images.is_empty() {
            eprintln!("{muted}[{tag}] 현재 첨부 대기 중인 스크린샷/이미지가 없습니다.\x1b[0m");
        } else {
            eprintln!(
                "{accent}[{tag}]\x1b[0m 현재 첨부 대기 중인 이미지 ({}개):",
                state.pending_images.len()
            );
            for (i, img) in state.pending_images.iter().enumerate() {
                eprintln!(
                    "  {accent}{:>2}.\x1b[0m {} ({:.1} KB) — {}",
                    i + 1,
                    img.mime_type,
                    img.byte_len as f64 / 1024.0,
                    img.path.display()
                );
            }
        }
        return None;
    }

    let candidate_path_str = trimmed.strip_prefix('@').unwrap_or(trimmed);
    if !candidate_path_str.is_empty()
        && let Some(img) = try_load_image_file(Path::new(candidate_path_str))
    {
        let kb = (img.byte_len as f64 / 1024.0).max(0.1);
        eprintln!(
            "{accent}[{tag}]\x1b[0m 📷 이미지 파일 첨부됨 (\x1b[1m#{}\x1b[0m, {}, {:.1} KB): {muted}{}\x1b[0m",
            state.pending_images.len() + 1,
            img.mime_type,
            kb,
            img.path.display()
        );
        state.pending_images.push(img);
        return None;
    }

    state.paste_seq += 1;
    match capture_clipboard_payload(state.paste_seq) {
        Ok(ClipboardPayload::Image(img)) => {
            let kb = (img.byte_len as f64 / 1024.0).max(0.1);
            eprintln!(
                "{accent}[{tag}]\x1b[0m 📷 클립보드 스크린샷 첨부됨 (\x1b[1m#{}\x1b[0m, {}, {:.1} KB): {muted}{}\x1b[0m",
                state.pending_images.len() + 1,
                img.mime_type,
                kb,
                img.path.display()
            );
            state.pending_images.push(img);
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
            eprintln!(
                "{muted}  질문을 입력하고 Enter를 누르거나, 바로 Enter를 눌러 스크린샷 분석을 실행하세요. (취소: /paste clear)\x1b[0m"
            );
            None
        }
        Ok(ClipboardPayload::Text(_)) => {
            eprintln!(
                "{warn}[{tag}]\x1b[0m 클립보드에 스크린샷 이미지가 없습니다 (현재 텍스트만 들어 있습니다). 화면을 캡처한 뒤 다시 실행하거나 `/paste <파일경로>`를 지정하세요."
            );
            None
        }
        Err(err) => {
            eprintln!("{warn}[{tag}]\x1b[0m 클립보드에서 스크린샷을 읽지 못했습니다: {err:#}");
            None
        }
    }
}

/// Wayland (`wl-paste`: 이미지 / `text/uri-list` / 텍스트) 및 X11 (`xclip`) 클립보드 통합 리더
pub fn capture_clipboard_payload(seq: usize) -> Result<ClipboardPayload> {
    if let Ok(types_out) = Command::new("wl-paste")
        .arg("--list-types")
        .stderr(Stdio::null())
        .output()
        && types_out.status.success()
    {
        let types_str = String::from_utf8_lossy(&types_out.stdout);
        let offered: Vec<&str> = types_str.lines().map(str::trim).collect();

        for &(mime, ext) in SUPPORTED_IMAGE_MIMES {
            if offered.iter().any(|t| t.eq_ignore_ascii_case(mime))
                && let Ok(img_out) = Command::new("wl-paste")
                    .arg("--type")
                    .arg(mime)
                    .stderr(Stdio::null())
                    .output()
                && img_out.status.success()
                && !img_out.stdout.is_empty()
            {
                let saved = save_clipboard_image_bytes(&img_out.stdout, mime, ext, seq)?;
                return Ok(ClipboardPayload::Image(saved));
            }
        }

        if offered.iter().any(|t| t.starts_with("text/uri-list"))
            && let Ok(uri_out) = Command::new("wl-paste")
                .arg("--type")
                .arg("text/uri-list")
                .stderr(Stdio::null())
                .output()
            && uri_out.status.success()
        {
            let uris = String::from_utf8_lossy(&uri_out.stdout);
            for line in uris.lines() {
                let cleaned = line.trim().strip_prefix("file://").unwrap_or(line.trim());
                if !cleaned.is_empty()
                    && let Some(img) = try_load_image_file(Path::new(cleaned))
                {
                    return Ok(ClipboardPayload::Image(img));
                }
            }
        }

        if let Ok(text_out) = Command::new("wl-paste")
            .arg("--no-newline")
            .stderr(Stdio::null())
            .output()
            && text_out.status.success()
            && !text_out.stdout.is_empty()
        {
            let text = String::from_utf8_lossy(&text_out.stdout).to_string();
            let trimmed_path = text.trim().strip_prefix("file://").unwrap_or(text.trim());
            if let Some(img) = try_load_image_file(Path::new(trimmed_path)) {
                return Ok(ClipboardPayload::Image(img));
            }
            return Ok(ClipboardPayload::Text(text));
        }
    }

    if let Ok(targets_out) = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .stderr(Stdio::null())
        .output()
        && targets_out.status.success()
    {
        let targets_str = String::from_utf8_lossy(&targets_out.stdout);
        for &(mime, ext) in SUPPORTED_IMAGE_MIMES {
            if targets_str.lines().any(|t| t.trim().eq_ignore_ascii_case(mime))
                && let Ok(img_out) = Command::new("xclip")
                    .args(["-selection", "clipboard", "-t", mime, "-o"])
                    .stderr(Stdio::null())
                    .output()
                && img_out.status.success()
                && !img_out.stdout.is_empty()
            {
                let saved = save_clipboard_image_bytes(&img_out.stdout, mime, ext, seq)?;
                return Ok(ClipboardPayload::Image(saved));
            }
        }

        if let Ok(text_out) = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .stderr(Stdio::null())
            .output()
            && text_out.status.success()
            && !text_out.stdout.is_empty()
        {
            let text = String::from_utf8_lossy(&text_out.stdout).to_string();
            if let Some(img) = try_load_image_file(Path::new(text.trim())) {
                return Ok(ClipboardPayload::Image(img));
            }
            return Ok(ClipboardPayload::Text(text));
        }
    }

    bail!("클립보드에 이미지나 텍스트가 없거나 클립보드 도구(wl-paste / xclip)를 호출할 수 없습니다.")
}

fn save_clipboard_image_bytes(
    bytes: &[u8],
    mime_type: &str,
    ext: &str,
    seq: usize,
) -> Result<ImageAttachment> {
    let dir = screenshots_dir();
    fs::create_dir_all(&dir).context("스크린샷 저장 디렉토리 생성 실패")?;

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("paste-{ts_ms}-{seq}.{ext}");
    let path = dir.join(filename);
    fs::write(&path, bytes)
        .with_context(|| format!("스크린샷 파일 저장 실패 ({})", path.display()))?;

    Ok(ImageAttachment {
        path,
        mime_type: mime_type.to_string(),
        base64_data: BASE64_STANDARD.encode(bytes),
        byte_len: bytes.len(),
    })
}

pub fn try_load_image_file(path: &Path) -> Option<ImageAttachment> {
    let expanded = if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix("~/")) {
        let home = std::env::var("HOME").ok()?;
        PathBuf::from(home).join(rest)
    } else {
        path.to_path_buf()
    };

    if !expanded.is_file() {
        return None;
    }

    let ext = expanded
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let mime_type = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };

    let bytes = fs::read(&expanded).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let abs_path = fs::canonicalize(&expanded).unwrap_or(expanded);

    Some(ImageAttachment {
        path: abs_path,
        mime_type: mime_type.to_string(),
        base64_data: BASE64_STANDARD.encode(&bytes),
        byte_len: bytes.len(),
    })
}

/// 프롬프트 내에 `@/path/to/image.png` 또는 `/path/to/image.png` 형태로 적힌 로컬 이미지 경로를 추출합니다.
pub fn extract_inline_image_attachments(prompt: &str) -> Vec<ImageAttachment> {
    let mut out = Vec::new();
    for token in prompt.split_whitespace() {
        let cleaned = token
            .trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';'))
            .strip_prefix('@')
            .unwrap_or(token);
        if (cleaned.starts_with('/') || cleaned.starts_with("~/") || cleaned.starts_with("./"))
            && let Some(img) = try_load_image_file(Path::new(cleaned))
            && !out.iter().any(|existing: &ImageAttachment| existing.path == img.path)
        {
            out.push(img);
        }
    }
    out
}

pub fn collect_turn_images(
    pending_images: &mut Vec<ImageAttachment>,
    prompt: &str,
) -> Vec<ImageAttachment> {
    let mut turn_images = std::mem::take(pending_images);
    for inline_img in extract_inline_image_attachments(prompt) {
        if !turn_images.iter().any(|existing| existing.path == inline_img.path) {
            turn_images.push(inline_img);
        }
    }
    turn_images
}

/// 첨부된 스크린샷 이미지 경로를 `agy` 프롬프트 상단에 결합합니다.
pub fn build_prompt_with_images(prompt: &str, images: &[ImageAttachment]) -> String {
    if images.is_empty() {
        return prompt.to_string();
    }

    let mut header = String::from(
        "[첨부된 스크린샷/이미지 파일 — 답변 전 `read_file` 도구로 아래 이미지 경로를 직접 열어 화면 내용을 확인하세요]\n",
    );
    for img in images {
        header.push_str(&format!("- @{}\n", img.path.display()));
    }
    header.push('\n');
    header.push_str(prompt);
    header
}

pub fn copy_to_system_clipboard(text: &str) -> bool {
    let mut ok = false;
    if let Ok(mut child) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if let Ok(status) = child.wait()
            && status.success()
        {
            ok = true;
        }
    }
    if !ok
        && let Ok(mut child) = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if let Ok(status) = child.wait()
            && status.success()
        {
            ok = true;
        }
    }
    // OSC 52 폴백: 원격 SSH 및 터미널 직접 클립보드 지원
    let b64 = BASE64_STANDARD.encode(text.as_bytes());
    print!("\x1b]52;c;{b64}\x07");
    let _ = io::stdout().flush();
    ok
}

/// `1:2-5` 또는 `1` 형태의 인덱스·줄 범위 인자를 파싱합니다.
fn parse_block_and_line_range(spec: &str) -> Option<(usize, Option<(usize, usize)>)> {
    let trimmed = spec.trim().strip_prefix('#').unwrap_or(spec.trim());
    if let Some((idx_str, range_str)) = trimmed.split_once(':') {
        let idx = idx_str.trim().parse::<usize>().ok()?;
        if let Some((s_str, e_str)) = range_str.split_once('-') {
            let start = s_str.trim().parse::<usize>().ok()?.max(1);
            let end = e_str.trim().parse::<usize>().ok()?.max(start);
            return Some((idx, Some((start, end))));
        } else {
            let line = range_str.trim().parse::<usize>().ok()?.max(1);
            return Some((idx, Some((line, line))));
        }
    }
    let idx = trimmed.parse::<usize>().ok()?;
    Some((idx, None))
}

fn slice_code_lines(code: &str, range: Option<(usize, usize)>) -> String {
    let Some((start, end)) = range else {
        return code.to_string();
    };
    let all_lines: Vec<&str> = code.lines().collect();
    if all_lines.is_empty() || start > all_lines.len() {
        return code.to_string();
    }
    let s_idx = start - 1;
    let e_idx = end.min(all_lines.len());
    format!("{}\n", all_lines[s_idx..e_idx].join("\n"))
}

/// 마우스로 드래그해 선택한 Primary Selection (`wl-paste --primary`)을 읽어옵니다.
fn read_primary_selection() -> Option<String> {
    if let Ok(out) = Command::new("wl-paste")
        .args(["--primary", "--no-newline"])
        .stderr(Stdio::null())
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    if let Ok(out) = Command::new("xclip")
        .args(["-selection", "primary", "-o"])
        .stderr(Stdio::null())
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

pub fn print_captured_blocks_status(state: &SharedReplState) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let tag = state.app_tag;

    if state.last_code_blocks.is_empty() && state.last_raw_response.is_empty() {
        eprintln!("{muted}[{tag}] 현재 캡처된 코드 블록이나 직전 응답이 없습니다.\x1b[0m");
        return;
    }

    if !state.last_code_blocks.is_empty() {
        eprintln!(
            "{accent}[{tag}]\x1b[0m 캡처된 코드 블록 목록 ({}개):",
            state.last_code_blocks.len()
        );
        for b in &state.last_code_blocks {
            let lang = if b.lang.is_empty() { "code" } else { &b.lang };
            let first_line = collapse_whitespace(b.code.lines().next().unwrap_or(""), 60);
            eprintln!(
                "  {accent}#{:<2}\x1b[0m {:<8} ({:>3} lines)  {muted}{first_line}\x1b[0m",
                b.index,
                lang,
                b.code.lines().count()
            );
        }
    } else {
        eprintln!("{muted}[{tag}] 직전 응답에 코드 블록은 없습니다.\x1b[0m");
    }

    if !state.last_raw_response.is_empty() {
        let lines = state.last_raw_response.lines().count();
        let preview = collapse_whitespace(&state.last_raw_response, 68);
        eprintln!(
            "{accent}[{tag}]\x1b[0m 직전 응답 전문 (`all`): {} chars ({} lines)  {muted}{preview}\x1b[0m",
            state.last_raw_response.len(),
            lines
        );
    }
    eprintln!(
        "{muted}  사용법: /c <번호> | /c <번호>:<시작줄>-<끝줄> | /c all | /quote <번호|all>\x1b[0m"
    );
}

pub fn copy_last_code_or_response(state: &SharedReplState, arg: Option<&str>) {
    let accent = state.active_flavor.accent_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    let target_arg = arg.unwrap_or("").trim();
    if target_arg.eq_ignore_ascii_case("list")
        || target_arg.eq_ignore_ascii_case("ls")
        || target_arg.eq_ignore_ascii_case("status")
    {
        print_captured_blocks_status(state);
        return;
    }

    if target_arg.eq_ignore_ascii_case("all")
        || target_arg.eq_ignore_ascii_case("full")
        || target_arg.eq_ignore_ascii_case("text")
    {
        if state.last_raw_response.is_empty() {
            eprintln!("{warn}[{tag}]\x1b[0m 복사할 직전 응답이 없습니다.");
            return;
        }
        copy_to_system_clipboard(&state.last_raw_response);
        eprintln!(
            "{accent}[{tag}]\x1b[0m 📋 직전 응답 마크다운 전문이 클립보드에 복사되었습니다 ({} chars, {} lines).",
            state.last_raw_response.len(),
            state.last_raw_response.lines().count()
        );
        let _ = Command::new("notify-send")
            .args([
                "-t",
                "1500",
                "-a",
                "agy",
                "📋 응답 복사 완료",
                "직전 응답 전문이 클립보드에 복사되었습니다.",
            ])
            .status();
        return;
    }

    if let Some((idx, line_range)) = parse_block_and_line_range(target_arg) {
        if let Some(block) = state.last_code_blocks.iter().find(|b| b.index == idx) {
            let sliced = slice_code_lines(&block.code, line_range);
            copy_to_system_clipboard(&sliced);
            let range_note = match line_range {
                Some((s, e)) => format!(" (줄 {s}~{e})"),
                None => String::new(),
            };
            eprintln!(
                "{accent}[{tag}]\x1b[0m 📋 #{idx}{range_note} 코드 블록({})이 클립보드에 복사되었습니다 ({} lines).",
                if block.lang.is_empty() { "code" } else { &block.lang },
                sliced.lines().count()
            );
            let _ = Command::new("notify-send")
                .args([
                    "-t",
                    "1500",
                    "-a",
                    "agy",
                    "📋 코드 복사 완료",
                    &format!("#{idx}{range_note} 코드 블록이 복사되었습니다."),
                ])
                .status();
            return;
        } else {
            eprintln!(
                "{warn}[{tag}]\x1b[0m #{idx}번 코드 블록을 찾을 수 없습니다. (현재 {}개 존재 — `/c list`로 확인)",
                state.last_code_blocks.len()
            );
            return;
        }
    }

    if let Some(block) = state.last_code_blocks.last() {
        let b_idx = block.index;
        copy_to_system_clipboard(&block.code);
        eprintln!(
            "{accent}[{tag}]\x1b[0m 📋 가장 최근 #{b_idx} 코드 블록({})이 클립보드에 복사되었습니다 ({} lines).",
            if block.lang.is_empty() { "code" } else { &block.lang },
            block.code.lines().count()
        );
        let _ = Command::new("notify-send")
            .args([
                "-t",
                "1500",
                "-a",
                "agy",
                "📋 코드 복사 완료",
                &format!("#{b_idx} 코드 블록이 복사되었습니다."),
            ])
            .status();
    } else if !state.last_raw_response.is_empty() {
        copy_to_system_clipboard(&state.last_raw_response);
        eprintln!("{accent}[{tag}]\x1b[0m 📋 직전 응답 전문이 클립보드에 복사되었습니다.");
        let _ = Command::new("notify-send")
            .args([
                "-t",
                "1500",
                "-a",
                "agy",
                "📋 복사 완료",
                "직전 응답 전문이 복사되었습니다.",
            ])
            .status();
    } else {
        eprintln!("{warn}[{tag}]\x1b[0m 아직 복사할 코드 블록이나 응답이 없습니다.");
    }
}

pub fn handle_quote_command(arg: &str, state: &mut SharedReplState, _editor: &mut Reedline) {
    state.sync_external_quotes();
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;
    let trimmed = arg.trim();

    if trimmed.eq_ignore_ascii_case("clear")
        || trimmed.eq_ignore_ascii_case("drop")
        || trimmed.eq_ignore_ascii_case("rm")
    {
        let n = state.pending_quotes.len();
        state.pending_quotes.clear();
        eprintln!("{accent}[{tag}]\x1b[0m 대기 중이던 인용 {n}개를 모두 비웠습니다.");
        return;
    }

    if trimmed.eq_ignore_ascii_case("list")
        || trimmed.eq_ignore_ascii_case("ls")
        || trimmed.eq_ignore_ascii_case("status")
    {
        if state.pending_quotes.is_empty() {
            eprintln!("{muted}[{tag}] 현재 대기 중인 인용 항목이 없습니다.\x1b[0m");
        } else {
            eprintln!(
                "{accent}[{tag}]\x1b[0m 현재 대기 중인 인용 목록 ({}개 — 다음 질문에 자동 첨부):",
                state.pending_quotes.len()
            );
            for (i, q) in state.pending_quotes.iter().enumerate() {
                let preview = collapse_whitespace(&q.content, 60);
                eprintln!(
                    "  {accent}{:>2}.\x1b[0m 💬 \x1b[1m{}\x1b[0m  {muted}{preview}\x1b[0m",
                    i + 1,
                    q.label
                );
            }
        }
        print_captured_blocks_status(state);
        return;
    }

    let (label, content) = if trimmed.eq_ignore_ascii_case("all")
        || trimmed.eq_ignore_ascii_case("full")
    {
        if state.last_raw_response.is_empty() {
            eprintln!("{warn}[{tag}]\x1b[0m 인용할 직전 응답이 없습니다.");
            return;
        }
        let lines = state.last_raw_response.lines().count();
        let quoted = state
            .last_raw_response
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        (format!("직전 응답 전문 ({lines} lines)"), quoted)
    } else if trimmed.eq_ignore_ascii_case("sel") || trimmed.eq_ignore_ascii_case("selection") {
        let Some(sel) = read_primary_selection() else {
            eprintln!("{warn}[{tag}]\x1b[0m 마우스로 드래그해 선택한 영역(Primary Selection)이 없습니다.");
            return;
        };
        let lines = sel.lines().count();
        let quoted = sel
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        (format!("마우스 선택 영역 ({lines} lines)"), quoted)
    } else if let Some((idx, line_range)) = parse_block_and_line_range(trimmed) {
        if let Some(block) = state.last_code_blocks.iter().find(|b| b.index == idx) {
            let sliced = slice_code_lines(&block.code, line_range);
            let lang = if block.lang.is_empty() { "code" } else { &block.lang };
            let range_note = match line_range {
                Some((s, e)) => format!(":{s}-{e}"),
                None => String::new(),
            };
            let label = format!(
                "#{idx}{range_note} {lang} ({} lines)",
                sliced.lines().count()
            );
            let content = format!("```{}\n{}\n```", block.lang, sliced.trim_end());
            (label, content)
        } else {
            eprintln!(
                "{warn}[{tag}]\x1b[0m #{idx}번 코드 블록을 찾을 수 없습니다. (현재 {}개 존재 — `/quote list`로 확인)",
                state.last_code_blocks.len()
            );
            return;
        }
    } else if trimmed.is_empty() {
        // 인자 없이 `/quote` 또는 `Alt+Q` 실행 시:
        // 1) 직전 응답 안의 문구를 마우스로 드래그해 둔 경우 해당 선택 영역 우선 인용
        // 2) 그렇지 않으면 가장 최근 코드 블록 -> 직전 응답 전문 순으로 인용
        if let Some(sel) = read_primary_selection()
            && !state.last_raw_response.is_empty()
            && sel.len() >= 4
            && state.last_raw_response.contains(sel.trim())
        {
            let lines = sel.lines().count();
            let quoted = sel
                .lines()
                .map(|l| format!("> {l}"))
                .collect::<Vec<_>>()
                .join("\n");
            (format!("마우스 드래그 선택 ({lines} lines)"), quoted)
        } else if let Some(block) = state.last_code_blocks.last() {
            let lang = if block.lang.is_empty() { "code" } else { &block.lang };
            let label = format!("#{} {lang} ({} lines)", block.index, block.code.lines().count());
            let content = format!("```{}\n{}\n```", block.lang, block.code.trim_end());
            (label, content)
        } else if !state.last_raw_response.is_empty() {
            let lines = state.last_raw_response.lines().count();
            let quoted = state
                .last_raw_response
                .lines()
                .map(|l| format!("> {l}"))
                .collect::<Vec<_>>()
                .join("\n");
            (format!("직전 응답 전문 ({lines} lines)"), quoted)
        } else {
            eprintln!("{warn}[{tag}]\x1b[0m 아직 인용할 코드 블록이나 응답이 없습니다.");
            return;
        }
    } else {
        eprintln!(
            "{warn}[{tag}]\x1b[0m 알 수 없는 `/quote` 인자입니다 ({trimmed}). 사용법: `/quote [번호|번호:시작-끝|all|sel|list|clear]`"
        );
        return;
    };

    let preview = collapse_whitespace(&content, 64);
    state.pending_quotes.push(PendingQuote {
        label: label.clone(),
        content,
    });
    eprintln!(
        "{accent}[{tag}]\x1b[0m 💬 인용 대기열 추가됨 (\x1b[1m#{}\x1b[0m, {label}): {muted}{preview}\x1b[0m",
        state.pending_quotes.len()
    );
    eprintln!(
        "{muted}  프롬프트에 [💬x{}] 배지가 표시됩니다. 질문을 입력해 전송하거나 `/quote list`, `/quote clear`를 쓰세요.\x1b[0m",
        state.pending_quotes.len()
    );
}

pub fn expand_inline_quotes(prompt: &str, code_blocks: &[CapturedCodeBlock]) -> String {
    let mut expanded = prompt.to_string();
    for block in code_blocks {
        let token = format!("@#{}", block.index);
        if expanded.contains(&token) {
            let quote_block = format!("\n```{}\n{}\n```\n", block.lang, block.code.trim_end());
            expanded = expanded.replace(&token, &quote_block);
        }
    }
    expanded
}

/// 턴 실행 직전: 외부 클릭 인용 큐 동기화 + `pending_quotes` 배지 출력 및 결합 + `@#1` 인라인 확장을 수행합니다.
pub fn consume_quotes_and_expand_prompt(prompt: &str, state: &mut SharedReplState) -> String {
    state.sync_external_quotes();
    let muted = state.active_flavor.muted_ansi();
    let mut expanded = expand_inline_quotes(prompt, &state.last_code_blocks);

    let quotes = std::mem::take(&mut state.pending_quotes);
    if !quotes.is_empty() {
        print!("  ");
        for (i, q) in quotes.iter().enumerate() {
            print!("{muted}💬 [#{} {}]\x1b[0m ", i + 1, q.label);
        }
        println!();
        let mut combined = String::new();
        for q in &quotes {
            combined.push_str(&q.content);
            combined.push_str("\n\n");
        }
        combined.push_str(&expanded);
        expanded = combined;
    }
    expanded
}

pub fn handle_model_command(arg: &str, state: &mut SharedReplState) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let tag = state.app_tag;

    if arg.is_empty() {
        let mode_str = match &state.pinned_model {
            Some(m) => format!("수동 고정 (\x1b[1m{m}\x1b[0m)"),
            None => format!(
                "10단계 자동 라우팅 ({}-* / {}-* / {}-*)",
                state.targets.hydrogen_base, state.targets.argon_base, state.targets.carbon_base
            ),
        };
        eprintln!("{accent}[{tag}]\x1b[0m 현재 모델 설정: {mode_str}");
        eprintln!("{muted}  사용법: /model auto (자동 라우팅 복귀) | /model <모델명 또는 약어>\x1b[0m");
        eprintln!(
            "{muted}  약어 예시: hydrogen-low, hydrogen-medium, hydrogen-high, hydrogen-xhigh, argon-low, argon-medium, argon-high, carbon-low, carbon-medium, carbon-high, sonnet, opus\x1b[0m"
        );
        return;
    }

    let lower = arg.to_ascii_lowercase();
    if matches!(lower.as_str(), "auto" | "reset" | "dynamic" | "default") {
        state.pinned_model = None;
        eprintln!(
            "{accent}[{tag}]\x1b[0m 모델 고정이 해제되었습니다 -> 10단계 스펙트럼 자동 라우팅으로 동작합니다."
        );
        return;
    }

    let resolved = match lower.as_str() {
        "hydrogen" | "hydrogen-high" => state.targets.variant("hydrogen", "high"),
        "hydrogen-low" | "flash-low" => state.targets.variant("hydrogen", "low"),
        "hydrogen-medium" | "flash" | "flash-medium" => state.targets.variant("hydrogen", "medium"),
        "hydrogen-xhigh" | "flash-xhigh" => state.targets.variant("hydrogen", "xhigh"),
        "flash-high" => state.targets.variant("hydrogen", "high"),
        "argon" | "argon-medium" => state.targets.variant("argon", "medium"),
        "argon-low" => state.targets.variant("argon", "low"),
        "argon-high" => state.targets.variant("argon", "high"),
        "carbon" | "carbon-medium" => state.targets.variant("carbon", "medium"),
        "carbon-low" => state.targets.variant("carbon", "low"),
        "carbon-high" => state.targets.variant("carbon", "high"),
        "sonnet" | "sonnet-medium" => "claude-sonnet-5-5-medium".to_string(),
        "sonnet-high" => "claude-sonnet-5-5-high".to_string(),
        "opus" | "opus-medium" => "claude-opus-5-5-medium".to_string(),
        "opus-high" => "claude-opus-5-5-high".to_string(),
        _ => arg.to_string(),
    };

    state.pinned_model = Some(resolved.clone());
    state.last_model = resolved.clone();
    eprintln!(
        "{accent}[{tag}]\x1b[0m 모델이 \x1b[1m{resolved}\x1b[0m(으)로 고정되었습니다. (자동 라우팅 복귀: `/model auto`)"
    );
}

pub fn handle_effort_command(arg: &str, state: &mut SharedReplState) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    if arg.is_empty() {
        let cur = state
            .pinned_effort
            .as_deref()
            .unwrap_or("auto (라우터 자동 결정)");
        eprintln!("{accent}[{tag}]\x1b[0m 현재 추론 깊이(effort): \x1b[1m{cur}\x1b[0m");
        eprintln!("{muted}  사용법: /effort <auto|low|medium|high|xhigh|max>\x1b[0m");
        return;
    }

    let lower = arg.to_ascii_lowercase();
    match lower.as_str() {
        "auto" | "reset" | "default" => {
            state.pinned_effort = None;
            eprintln!("{accent}[{tag}]\x1b[0m 추론 깊이(effort)가 자동 결정 모드로 복귀했습니다.");
        }
        "low" | "medium" | "high" | "xhigh" | "max" => {
            state.pinned_effort = Some(lower.clone());
            eprintln!(
                "{accent}[{tag}]\x1b[0m 추론 깊이(effort)가 \x1b[1m{lower}\x1b[0m(으)로 고정되었습니다. (해제: `/effort auto`)"
            );
        }
        _ => {
            eprintln!(
                "{warn}[{tag}]\x1b[0m 지원하지 않는 effort 값입니다 ({arg}). 사용 가능: auto, low, medium, high, xhigh, max"
            );
        }
    }
}

/// 대화형 `/resume` 선택기: 로컬 DB·트랜스크립트 이력과 추가 후보(예: ACP `session/list`)를 병합해 번호·ID·검색어·전체 토글(`a`)을 지원합니다.
pub fn select_conversation_interactive(
    arg: &str,
    state: &SharedReplState,
    extra_rows: &[ConversationRow],
) -> Option<String> {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    let mut all_rows = fetch_recent_conversations(40);
    for extra in extra_rows {
        if !all_rows
            .iter()
            .any(|r| r.conversation_id == extra.conversation_id)
        {
            all_rows.push(extra.clone());
        }
    }

    if all_rows.is_empty() {
        eprintln!("{warn}[{tag}]\x1b[0m 복원할 수 있는 최근 agy 대화 기록을 찾지 못했습니다.");
        return None;
    }

    let cwd_uri = std::env::current_dir()
        .ok()
        .map(|p| format!("file://{}", p.display()));

    let arg_trimmed = arg.trim();

    if !arg_trimmed.is_empty() && !arg_trimmed.eq_ignore_ascii_case("all") {
        let workspace_rows = filter_rows_for_workspace(&all_rows, cwd_uri.as_deref());
        let target_pool = if workspace_rows.is_empty() {
            &all_rows
        } else {
            &workspace_rows
        };

        if let Ok(idx) = arg_trimmed.parse::<usize>()
            && idx >= 1
            && idx <= target_pool.len()
        {
            return Some(target_pool[idx - 1].conversation_id.clone());
        }

        if let Some(found) = all_rows
            .iter()
            .find(|r| r.conversation_id.starts_with(arg_trimmed))
            .or_else(|| {
                let q = arg_trimmed.to_lowercase();
                all_rows.iter().find(|r| {
                    r.title.to_lowercase().contains(&q) || r.preview.to_lowercase().contains(&q)
                })
            })
        {
            return Some(found.conversation_id.clone());
        }

        eprintln!(
            "{warn}[{tag}]\x1b[0m '{arg_trimmed}'와 일치하는 대화를 찾지 못했습니다. 전체 목록을 표시합니다."
        );
    }

    let mut show_all = arg_trimmed.eq_ignore_ascii_case("all");
    let workspace_rows = filter_rows_for_workspace(&all_rows, cwd_uri.as_deref());
    if workspace_rows.is_empty() {
        show_all = true;
    }

    loop {
        let display_rows: Vec<&ConversationRow> = if show_all {
            all_rows.iter().take(15).collect()
        } else {
            workspace_rows.iter().take(15).collect()
        };

        let scope_label = if show_all {
            "전체 워크스페이스"
        } else {
            "현재 워크스페이스"
        };
        eprintln!(
            "{accent}[{tag}]\x1b[0m 최근 대화 목록 ({scope_label}, 총 {}개 표시):",
            display_rows.len()
        );

        for (i, row) in display_rows.iter().enumerate() {
            let short_id = &row.conversation_id[..row.conversation_id.len().min(8)];
            let current_mark =
                if state.active_conv_id.as_deref() == Some(row.conversation_id.as_str()) {
                    "*"
                } else {
                    " "
                };
            let summary_text = summarize_row_title(row);
            let time_short = format_timestamp_short(&row.last_modified_time);
            let ws_short = format_workspace_short(&row.workspace_uris);

            if show_all && !ws_short.is_empty() {
                eprintln!(
                    "  {accent}{:>2}{current_mark}\x1b[0m \x1b[1m{short_id}\x1b[0m  {muted}{time_short} ({:>3} steps, {ws_short})\x1b[0m  {summary_text}",
                    i + 1,
                    row.step_count
                );
            } else {
                eprintln!(
                    "  {accent}{:>2}{current_mark}\x1b[0m \x1b[1m{short_id}\x1b[0m  {muted}{time_short} ({:>3} steps)\x1b[0m  {summary_text}",
                    i + 1,
                    row.step_count
                );
            }
        }

        let toggle_hint = if !show_all && all_rows.len() > workspace_rows.len() {
            ", a: 전체 워크스페이스 보기"
        } else {
            ""
        };
        eprint!(
            "{muted}  이어할 번호 또는 ID 입력 (Enter: 1번, q: 취소{toggle_hint}): \x1b[0m"
        );
        let _ = io::stderr().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return None;
        }
        let sel = input.trim();
        if sel.eq_ignore_ascii_case("q")
            || sel.eq_ignore_ascii_case("quit")
            || sel.eq_ignore_ascii_case("cancel")
        {
            eprintln!("{muted}  [취소됨]\x1b[0m");
            return None;
        }
        if sel.eq_ignore_ascii_case("a") || sel.eq_ignore_ascii_case("all") {
            show_all = true;
            continue;
        }
        if sel.is_empty() {
            return display_rows.first().map(|r| r.conversation_id.clone());
        }
        if let Ok(idx) = sel.parse::<usize>()
            && idx >= 1
            && idx <= display_rows.len()
        {
            return Some(display_rows[idx - 1].conversation_id.clone());
        }
        if let Some(found) = all_rows.iter().find(|r| r.conversation_id.starts_with(sel)) {
            return Some(found.conversation_id.clone());
        }

        eprintln!(
            "{warn}  유효한 번호(1~{}) 또는 세션 ID 앞자리를 입력해 주세요.\x1b[0m",
            display_rows.len()
        );
        return None;
    }
}

pub fn activate_resumed_conversation_state(conv_id: &str, state: &mut SharedReplState) {
    state.active_conv_id = Some(conv_id.to_string());
    state.use_continue_flag = false;
    state.turn_idx = count_user_turns_in_transcript(conv_id);
    state.hydrate_from_transcript(conv_id);
    print_conversation_resume_preview(state.app_tag, conv_id, state.active_flavor);
}

pub fn print_conversation_resume_preview(tag: &str, conv_id: &str, flavor: ThemeFlavor) {
    let accent = flavor.accent_ansi();
    let muted = flavor.muted_ansi();
    let short_id = &conv_id[..conv_id.len().min(8)];

    let turns = count_user_turns_in_transcript(conv_id);
    eprintln!("{accent}[{tag}]\x1b[0m 대화 세션 연결됨: \x1b[1m{conv_id}\x1b[0m (기존 {turns}턴)");

    if let Some((last_user, last_agent)) = read_last_exchange_from_transcript(conv_id) {
        eprintln!("{muted}  ┌── [직전 대화 맥락 ({short_id})]\x1b[0m");
        if !last_user.is_empty() {
            let user_one_line = collapse_whitespace(&last_user, 110);
            eprintln!("  {muted}│ 질문:\x1b[0m {user_one_line}");
        }
        if !last_agent.is_empty() {
            let agent_one_line = collapse_whitespace(&last_agent, 130);
            eprintln!("  {muted}│ 응답:\x1b[0m {agent_one_line}");
        }
        eprintln!("{muted}  └──\x1b[0m");
    }
}

pub fn run_git_diff_summary(tag: &str, flavor: ThemeFlavor) {
    let accent = flavor.accent_ansi();
    let muted = flavor.muted_ansi();

    let status_out = Command::new("git").args(["status", "-sb"]).output();
    let Ok(status) = status_out else {
        eprintln!("[{tag}] git 실행에 실패했습니다.");
        return;
    };
    if !status.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&status.stderr).trim());
        return;
    }

    eprintln!("{accent}[{tag}] Git 작업 트리 상태:\x1b[0m");
    print!("{}", String::from_utf8_lossy(&status.stdout));

    if let Ok(diff_stat) = Command::new("git")
        .args(["diff", "--stat", "--color=always"])
        .output()
        && diff_stat.status.success()
        && !diff_stat.stdout.is_empty()
    {
        eprintln!("{muted}── 변경 통계 (unstaged) ──\x1b[0m");
        print!("{}", String::from_utf8_lossy(&diff_stat.stdout));
    }
}

pub fn handle_artifact_command(arg: &str, state: &SharedReplState) {
    let accent = state.active_flavor.accent_ansi();
    let muted = state.active_flavor.muted_ansi();
    let warn = state.active_flavor.warn_ansi();
    let tag = state.app_tag;

    let Some(conv_id) = state.active_conv_id.as_deref() else {
        eprintln!(
            "{warn}[{tag}]\x1b[0m 활성 대화 세션이 없습니다. 먼저 대화를 시작하거나 `/resume`으로 세션을 선택하세요."
        );
        return;
    };

    let brain_dir = agy_brain_dir().join(conv_id);
    if !brain_dir.is_dir() {
        eprintln!(
            "{warn}[{tag}]\x1b[0m 현재 세션({conv_id})에 생성된 아티팩트 디렉토리가 없습니다."
        );
        return;
    }

    let mut files: Vec<PathBuf> = fs::read_dir(&brain_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    files.sort();

    if files.is_empty() {
        eprintln!("{muted}[{tag}] 현재 세션({conv_id})에 생성된 아티팩트 파일이 없습니다.\x1b[0m");
        return;
    }

    let trimmed = arg.trim();
    if !trimmed.is_empty() {
        let target = if let Ok(idx) = trimmed.parse::<usize>()
            && idx >= 1
            && idx <= files.len()
        {
            Some(&files[idx - 1])
        } else {
            files.iter().find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(trimmed))
            })
        };

        if let Some(path) = target {
            if let Ok(content) = fs::read_to_string(path) {
                eprintln!("{accent}[{tag}] 아티팩트 열람: {}\x1b[0m", path.display());
                let skin = build_skin_for_flavor(state.active_flavor);
                print!("{}", skin.term_text(&content));
            } else {
                eprintln!(
                    "{warn}[{tag}]\x1b[0m 파일을 읽을 수 없습니다: {}",
                    path.display()
                );
            }
            return;
        }
    }

    eprintln!(
        "{accent}[{tag}]\x1b[0m 세션 아티팩트 목록 ({}):",
        brain_dir.display()
    );
    for (i, f) in files.iter().enumerate() {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        eprintln!(
            "  {accent}{:>2}.\x1b[0m {name}  {muted}({})\x1b[0m",
            i + 1,
            f.display()
        );
    }
    eprintln!("{muted}  본문 보기: /artifact <번호 또는 파일명>\x1b[0m");
}

pub fn extract_file_path_from_info(tool_info: Option<&serde_json::Value>) -> Option<String> {
    let info = tool_info?;
    let params = info.get("parameters").or(Some(info));
    for key in ["Path", "path", "FilePath", "file_path", "target"] {
        if let Some(val) = params.and_then(|p| p.get(key)).and_then(|v| v.as_str()) {
            let trimmed = val.trim().trim_matches('"');
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn extract_tool_target_label(
    tool_name: &str,
    tool_info: Option<&serde_json::Value>,
) -> Option<String> {
    let info = tool_info?;
    let params = info.get("parameters").or(Some(info));
    match tool_name {
        "read_file" | "view_file" | "write_to_file" | "write_file" | "edit_file"
        | "replace_file_content" => extract_file_path_from_info(Some(info)).map(|p| {
            Path::new(&p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&p)
                .to_string()
        }),
        "run_command" => {
            if let Some(summary) = params
                .and_then(|p| p.get("toolSummary"))
                .and_then(|v| v.as_str())
            {
                let trimmed = summary.trim().trim_matches('"');
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            if let Some(cmd) = params
                .and_then(|p| p.get("CommandLine"))
                .and_then(|v| v.as_str())
            {
                let trimmed = cmd.trim().trim_matches('"');
                let first_word = trimmed.split_whitespace().next().unwrap_or(trimmed);
                return Some(first_word.to_string());
            }
            None
        }
        "grep_search" | "find_by_name" => {
            if let Some(q) = params
                .and_then(|p| p.get("query").or_else(|| p.get("pattern")))
                .and_then(|v| v.as_str())
            {
                return Some(q.trim().to_string());
            }
            None
        }
        _ => None,
    }
}

pub fn format_active_tool_label(tool_name: &str, target: Option<&str>) -> String {
    let clean_target = target.map(|t| {
        let trimmed = t.trim().trim_matches('"');
        // 긴 명령어 또는 파이프라인의 경우 첫 번째 주요 명령어나 핵심 요약만 36자 이내로 축약
        if trimmed.contains(" && ") || trimmed.contains(" | ") || trimmed.len() > 36 {
            let first_segment = trimmed.split(" && ").next().unwrap_or(trimmed);
            let first_word = first_segment.split_whitespace().next().unwrap_or(first_segment);
            let shortened = if first_word.len() > 32 {
                format!("{}…", &first_word[..32])
            } else {
                first_word.to_string()
            };
            collapse_whitespace(&shortened, 36)
        } else {
            collapse_whitespace(trimmed, 36)
        }
    });

    match tool_name {
        "read_file" | "view_file" => {
            if let Some(t) = clean_target {
                format!("파일 읽는 중 ({t})")
            } else {
                "파일 읽는 중...".to_string()
            }
        }
        "write_to_file" | "replace_file_content" | "multi_replace_file_content" | "sed_file"
        | "write_file" | "edit_file" => {
            if let Some(t) = clean_target {
                format!("파일 수정 중 ({t})")
            } else {
                "파일 수정 중...".to_string()
            }
        }
        "run_command" => {
            if let Some(t) = clean_target {
                format!("명령 실행 중 ({t})")
            } else {
                "명령 실행 중...".to_string()
            }
        }
        "grep_search" | "find_by_name" | "list_dir" => {
            if let Some(t) = clean_target {
                format!("코드베이스 탐색 중 ({t})")
            } else {
                "코드베이스 탐색 중...".to_string()
            }
        }
        _ => {
            if let Some(t) = clean_target {
                format!("{tool_name} 실행 중 ({t})")
            } else {
                format!("{tool_name} 실행 중...")
            }
        }
    }
}

pub fn format_cumulative_tool_summary(
    command_count: usize,
    edit_count: usize,
    read_count: usize,
    other_count: usize,
) -> String {
    let mut parts = Vec::new();
    if command_count > 0 {
        parts.push(format!("명령 {command_count}회"));
    }
    if edit_count > 0 {
        parts.push(format!("파일 {edit_count}개 수정"));
    }
    if read_count > 0 {
        parts.push(format!("파일 {read_count}개 열람"));
    }
    if other_count > 0 {
        parts.push(format!("도구 {other_count}회"));
    }
    if parts.is_empty() {
        "도구 작업 완료".to_string()
    } else {
        parts.join(", ")
    }
}

pub fn agy_app_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".gemini/antigravity-cli")
}

pub fn agy_brain_dir() -> PathBuf {
    agy_app_data_dir().join("brain")
}

pub fn state_base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/state/agy-decision-lab")
}

pub fn screenshots_dir() -> PathBuf {
    state_base_dir().join("screenshots")
}

pub fn fetch_recent_conversations(limit: usize) -> Vec<ConversationRow> {
    let db_path = agy_app_data_dir().join("conversation_summaries.db");
    if db_path.is_file() {
        let sql = format!(
            "SELECT conversation_id, title, substr(replace(preview, char(10), ' '), 1, 120) AS preview, step_count, last_modified_time, workspace_uris \
             FROM conversation_summaries \
             WHERE nesting_depth = 0 AND step_count > 0 \
             ORDER BY last_modified_time DESC LIMIT {limit};"
        );
        if let Ok(out) = Command::new("sqlite3")
            .arg("-json")
            .arg(&db_path)
            .arg(&sql)
            .stderr(Stdio::null())
            .output()
            && out.status.success()
            && let Ok(rows) = serde_json::from_slice::<Vec<ConversationRow>>(&out.stdout)
            && !rows.is_empty()
        {
            return rows;
        }
    }

    let brain = agy_brain_dir();
    let mut entries: Vec<(SystemTime, String)> = fs::read_dir(&brain)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, name))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));

    entries
        .into_iter()
        .take(limit)
        .map(|(_, cid)| {
            let preview = read_last_exchange_from_transcript(&cid)
                .map(|(u, _)| u)
                .unwrap_or_default();
            ConversationRow {
                conversation_id: cid,
                title: String::new(),
                preview,
                step_count: 0,
                last_modified_time: String::new(),
                workspace_uris: String::new(),
            }
        })
        .collect()
}

pub fn filter_rows_for_workspace(
    rows: &[ConversationRow],
    cwd_uri: Option<&str>,
) -> Vec<ConversationRow> {
    let Some(uri) = cwd_uri else {
        return rows.to_vec();
    };
    rows.iter()
        .filter(|r| r.workspace_uris.contains(uri))
        .cloned()
        .collect()
}

pub fn resolve_most_recent_conversation_id() -> Option<String> {
    let rows = fetch_recent_conversations(20);
    let cwd_uri = std::env::current_dir()
        .ok()
        .map(|p| format!("file://{}", p.display()));
    let ws_rows = filter_rows_for_workspace(&rows, cwd_uri.as_deref());
    ws_rows
        .first()
        .or_else(|| rows.first())
        .map(|r| r.conversation_id.clone())
}

pub fn resolve_conversation_id_prefix(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim();
    if trimmed.len() >= 36 {
        return Some(trimmed.to_string());
    }
    let rows = fetch_recent_conversations(100);
    rows.into_iter()
        .find(|r| r.conversation_id.starts_with(trimmed))
        .map(|r| r.conversation_id)
}

fn transcript_path_for(conv_id: &str) -> PathBuf {
    let base = agy_brain_dir().join(conv_id).join(".system_generated/logs");
    let full = base.join("transcript_full.jsonl");
    if full.is_file() {
        return full;
    }
    base.join("transcript.jsonl")
}

pub fn count_user_turns_in_transcript(conv_id: &str) -> usize {
    let path = transcript_path_for(conv_id);
    let Ok(content) = fs::read_to_string(path) else {
        return 0;
    };
    content
        .lines()
        .filter(|line| line.contains("\"USER_INPUT\""))
        .count()
}

pub fn read_last_exchange_from_transcript(conv_id: &str) -> Option<(String, String)> {
    let path = transcript_path_for(conv_id);
    let content = fs::read_to_string(path).ok()?;
    let mut last_user = String::new();
    let mut last_agent = String::new();
    let mut prev_agent = String::new();

    for line in content.lines() {
        let Ok(rec) = serde_json::from_str::<TranscriptStepRecord>(line) else {
            continue;
        };
        match rec.step_type.as_str() {
            "USER_INPUT" => {
                let extracted = extract_user_request_text(&rec.content);
                if !extracted.is_empty() {
                    last_user = extracted;
                    if !last_agent.is_empty() {
                        prev_agent = std::mem::take(&mut last_agent);
                    }
                }
            }
            "PLANNER_RESPONSE" => {
                let trimmed = rec.content.trim();
                if !trimmed.is_empty() {
                    last_agent = trimmed.to_string();
                }
            }
            _ => {}
        }
    }

    let effective_agent = if !last_agent.is_empty() {
        last_agent
    } else {
        prev_agent
    };

    if last_user.is_empty() && effective_agent.is_empty() {
        None
    } else {
        Some((last_user, effective_agent))
    }
}

/// 트랜스크립트에서 직전 응답 전문과, 최근 턴들에서 출력된 코드 블록들을 온전하게 복원합니다.
pub fn extract_turn_context_and_code_blocks_from_transcript(
    conv_id: &str,
) -> (String, Vec<CapturedCodeBlock>) {
    let path = transcript_path_for(conv_id);
    let Ok(content) = fs::read_to_string(path) else {
        return (String::new(), Vec::new());
    };

    let mut planner_responses: Vec<String> = Vec::new();

    for line in content.lines() {
        let Ok(rec) = serde_json::from_str::<TranscriptStepRecord>(line) else {
            continue;
        };
        if rec.step_type == "PLANNER_RESPONSE" {
            let trimmed = rec.content.trim();
            if !trimmed.is_empty() {
                planner_responses.push(trimmed.to_string());
            }
        }
    }

    let last_raw = planner_responses.last().cloned().unwrap_or_default();

    // 1. 가장 최근 응답에 코드 블록이 있으면 우선 추출
    let mut blocks = extract_code_blocks_from_markdown(&last_raw);

    // 2. 만약 가장 최근 응답에 코드 블록이 없다면, 역순으로 거슬러 올라가며 가장 최근에 코드를 포함했던 응답들에서 복원
    if blocks.is_empty() {
        for resp in planner_responses.iter().rev().skip(1) {
            if resp.contains("```") {
                blocks = extract_code_blocks_from_markdown(resp);
                if !blocks.is_empty() {
                    break;
                }
            }
        }
    }

    (last_raw, blocks)
}

pub fn extract_user_request_text(raw: &str) -> String {
    if let Some(start) = raw.find("<USER_REQUEST>")
        && let Some(end) = raw.find("</USER_REQUEST>")
        && end > start + "<USER_REQUEST>".len()
    {
        return raw[start + "<USER_REQUEST>".len()..end].trim().to_string();
    }
    raw.trim().to_string()
}

fn summarize_row_title(row: &ConversationRow) -> String {
    let base = if !row.title.trim().is_empty() && !row.preview.trim().is_empty() {
        if row.title.trim() == row.preview.trim() {
            row.title.trim().to_string()
        } else {
            format!("{} — {}", row.title.trim(), row.preview.trim())
        }
    } else if !row.title.trim().is_empty() {
        row.title.trim().to_string()
    } else if !row.preview.trim().is_empty() {
        row.preview.trim().to_string()
    } else {
        "(제목 없음)".to_string()
    };
    collapse_whitespace(&base, 68)
}

fn format_timestamp_short(raw: &str) -> String {
    let s = raw.trim();
    if s.len() >= 16 {
        s[5..16].to_string()
    } else if s.is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

fn format_workspace_short(raw_uris: &str) -> String {
    let Ok(uris) = serde_json::from_str::<Vec<String>>(raw_uris) else {
        return String::new();
    };
    let Some(first) = uris.first() else {
        return String::new();
    };
    let path_str = first.strip_prefix("file://").unwrap_or(first);
    Path::new(path_str)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path_str)
        .to_string()
}

pub fn collapse_whitespace(s: &str, max_chars: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

pub fn read_single_prompt(
    cli_prompt: Option<String>,
    has_images: bool,
    app_name: &str,
) -> Result<String> {
    if let Some(p) = cli_prompt
        && !p.trim().is_empty()
    {
        return Ok(p);
    }

    if !io::stdin().is_terminal() {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .context("stdin에서 프롬프트를 읽는 데 실패했습니다")?;
        if !buf.trim().is_empty() {
            return Ok(buf);
        }
    }

    if has_images {
        return Ok(DEFAULT_IMAGE_ANALYSIS_PROMPT.to_string());
    }

    bail!("프롬프트를 인자로 전달하거나 대화형 터미널에서 `{app_name}`를 실행해 주세요.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_user_request_block_cleanly() {
        let raw =
            "<USER_REQUEST>\n안녕하세요 테스트입니다\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\n...";
        assert_eq!(extract_user_request_text(raw), "안녕하세요 테스트입니다");
    }

    #[test]
    fn applies_effort_override_on_baked_and_unbaked_models() {
        let (m1, f1) = apply_effort_override("gemini-4-argon-medium", Some("high"));
        assert_eq!(m1, "gemini-4-argon-high");
        assert!(f1.is_none());

        let (m2, f2) = apply_effort_override("gemini-4-carbon-high", Some("max"));
        assert_eq!(m2, "gemini-4-carbon-high");
        assert_eq!(f2.as_deref(), Some("max"));
    }

    #[test]
    fn builds_prompt_with_attached_images() {
        let img = ImageAttachment {
            path: PathBuf::from("/tmp/paste-test.png"),
            mime_type: "image/png".to_string(),
            base64_data: "AAAA".to_string(),
            byte_len: 3,
        };
        let combined = build_prompt_with_images("이 에러가 왜 나는지 봐줘", &[img]);
        assert!(combined.contains("@/tmp/paste-test.png"));
        assert!(combined.contains("이 에러가 왜 나는지 봐줘"));
    }

    #[test]
    fn expands_inline_code_block_quotes() {
        let blocks = vec![CapturedCodeBlock {
            index: 1,
            lang: "rust".to_string(),
            code: "fn main() {}\n".to_string(),
            id: "abc".to_string(),
        }];
        let out = expand_inline_quotes("@#1 이 함수를 리팩터링해줘", &blocks);
        assert!(out.contains("```rust\nfn main() {}\n```"));
        assert!(out.contains("이 함수를 리팩터링해줘"));
    }

    #[test]
    fn hydrates_code_blocks_from_prior_turn_when_last_response_lacks_code() {
        let temp_dir = std::env::temp_dir().join(format!("agy_test_conv_{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let logs_dir = temp_dir.join(".system_generated/logs");
        fs::create_dir_all(&logs_dir).unwrap();

        let jsonl = r#"{"type":"USER_INPUT","content":"<USER_REQUEST>\n코드 작성해줘\n</USER_REQUEST>"}
{"type":"PLANNER_RESPONSE","content":"작성했습니다:\n```rust\nfn add(a: i32, b: i32) -> i32 { a + b }\n```"}
{"type":"USER_INPUT","content":"<USER_REQUEST>\n고마워\n</USER_REQUEST>"}
{"type":"PLANNER_RESPONSE","content":"천만에요! 더 필요하신 점이 있으신가요?"}
"#;
        fs::write(logs_dir.join("transcript.jsonl"), jsonl).unwrap();

        let conv_id = temp_dir.file_name().unwrap().to_str().unwrap();
        // agy_brain_dir 대신 직접 함수 검증을 위해 임시 디렉토리를 symlink 또는 path_for 테스트
        let target_brain = agy_brain_dir().join(conv_id);
        let _ = fs::create_dir_all(target_brain.parent().unwrap());
        let _ = std::os::unix::fs::symlink(&temp_dir, &target_brain);

        let (last_resp, blocks) = extract_turn_context_and_code_blocks_from_transcript(conv_id);
        assert_eq!(last_resp, "천만에요! 더 필요하신 점이 있으신가요?");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, "rust");
        assert!(blocks[0].code.contains("fn add(a: i32, b: i32) -> i32"));

        let _ = fs::remove_file(&target_brain);
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ctrl_j_and_alt_enter_insert_newline_instead_of_submitting_in_all_modes() {
        let expected_newline = ReedlineEvent::Edit(vec![EditCommand::InsertNewline]);

        let emacs_kb = build_emacs_keybindings();
        assert_eq!(
            emacs_kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('j')),
            Some(expected_newline.clone())
        );
        assert_eq!(
            emacs_kb.find_binding(KeyModifiers::ALT, KeyCode::Enter),
            Some(expected_newline.clone())
        );
        assert_eq!(
            emacs_kb.find_binding(KeyModifiers::SHIFT, KeyCode::Enter),
            Some(expected_newline.clone())
        );
        // 기본 Enter는 여전히 프롬프트 전송(ReedlineEvent::Enter)을 유지해야 함
        assert_eq!(
            emacs_kb.find_binding(KeyModifiers::NONE, KeyCode::Enter),
            Some(ReedlineEvent::Enter)
        );

        let (vi_insert_kb, vi_normal_kb, _) = build_vi_keybindings();
        assert_eq!(
            vi_insert_kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('j')),
            Some(expected_newline.clone())
        );
        assert_eq!(
            vi_normal_kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('j')),
            Some(expected_newline)
        );
    }
}
