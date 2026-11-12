use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    process::{Command, Stdio},
    time::SystemTime,
};

use termimad::{
    CompoundStyle, MadSkin, STANDARD_TABLE_BORDER_CHARS, StyledChar,
    crossterm::style::{Attribute, Color},
};

/// 감지되거나 사용자가 지정한 터미널 테마 밝기 모드
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeFlavor {
    Light,
    Dark,
}

impl ThemeFlavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// 배지·강조 텍스트용 ANSI 이스케이프 코드
    pub fn accent_ansi(self) -> &'static str {
        match self {
            Self::Light => "\x1b[38;5;25m",
            Self::Dark => "\x1b[38;5;81m",
        }
    }

    /// 보조 안내·토큰 요약용 디밍 ANSI 이스케이프 코드
    pub fn muted_ansi(self) -> &'static str {
        match self {
            Self::Light => "\x1b[38;5;241m",
            Self::Dark => "\x1b[38;5;245m",
        }
    }

    /// 경고 메시지용 ANSI 이스케이프 코드
    pub fn warn_ansi(self) -> &'static str {
        match self {
            Self::Light => "\x1b[38;5;130m",
            Self::Dark => "\x1b[38;5;214m",
        }
    }

    /// 성공/체크 표시용 ANSI 이스케이프 코드
    pub fn success_ansi(self) -> &'static str {
        match self {
            Self::Light => "\x1b[38;5;28m",
            Self::Dark => "\x1b[38;5;71m",
        }
    }

    /// 말풍선 버블 배경 및 테두리 ANSI 이스케이프 코드
    pub fn bubble_border_ansi(self) -> &'static str {
        match self {
            Self::Light => "\x1b[38;5;248m",
            Self::Dark => "\x1b[38;5;240m",
        }
    }

    /// bat 코드 하이라이팅 기본 테마 (사용자가 BAT_THEME을 지정하지 않은 경우)
    pub fn default_bat_theme(self) -> &'static str {
        match self {
            Self::Light => "OneHalfLight",
            Self::Dark => "OneHalfDark",
        }
    }
}

/// `--theme auto|light|dark` 설정과 실제 터미널 배경 휘도(OSC 11 -> $COLORFGBG -> OS 다크모드 폴백)를 결합해 테마를 결정합니다.
pub fn resolve_theme_flavor(preference: &str) -> (ThemeFlavor, Option<f32>) {
    match preference.trim().to_ascii_lowercase().as_str() {
        "light" => return (ThemeFlavor::Light, None),
        "dark" => return (ThemeFlavor::Dark, None),
        _ => {}
    }

    if io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && let Ok(luma) = terminal_light::luma()
    {
        let flavor = if luma > 0.52 {
            ThemeFlavor::Light
        } else {
            ThemeFlavor::Dark
        };
        return (flavor, Some(luma));
    }

    if let Some(os_flavor) = detect_os_color_scheme() {
        return (os_flavor, None);
    }

    (ThemeFlavor::Dark, None)
}

fn detect_os_color_scheme() -> Option<ThemeFlavor> {
    if cfg!(target_os = "macos") {
        let out = Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        return if out.status.success() {
            Some(ThemeFlavor::Dark)
        } else {
            Some(ThemeFlavor::Light)
        };
    }

    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    if text.contains("prefer-dark") {
        Some(ThemeFlavor::Dark)
    } else if text.contains("prefer-light") || text.contains("default") {
        Some(ThemeFlavor::Light)
    } else {
        None
    }
}

/// 감지된 Light / Dark 환경 각각에서 높은 명도 대비를 보장하는 맞춤형 마크다운 스킨을 생성합니다.
pub fn build_skin_for_flavor(flavor: ThemeFlavor) -> MadSkin {
    let mut skin = match flavor {
        ThemeFlavor::Light => MadSkin::default_light(),
        ThemeFlavor::Dark => MadSkin::default_dark(),
    };

    let (accent, code_fg, border_fg, code_block_fg) = match flavor {
        ThemeFlavor::Light => (
            Color::AnsiValue(25),  // 짙은 로열 블루
            Color::AnsiValue(125), // 짙은 자주색
            Color::AnsiValue(242), // 진한 슬레이트 그레이
            Color::AnsiValue(23),  // 짙은 청록
        ),
        ThemeFlavor::Dark => (
            Color::AnsiValue(81),  // 밝은 스카이 시안
            Color::AnsiValue(215), // 따뜻한 앰버 오렌지
            Color::AnsiValue(245), // 밝은 실버 그레이
            Color::AnsiValue(152), // 부드러운 민트 블루
        ),
    };

    skin.paragraph.compound_style = CompoundStyle::default();
    skin.bold = CompoundStyle::new(None, None, Attribute::Bold.into());
    skin.italic = CompoundStyle::new(None, None, Attribute::Italic.into());
    skin.strikeout = CompoundStyle::new(None, None, Attribute::CrossedOut.into());
    skin.inline_code = CompoundStyle::new(Some(code_fg), None, Attribute::Bold.into());
    skin.code_block.compound_style =
        CompoundStyle::new(Some(code_block_fg), None, Default::default());

    for h in &mut skin.headers {
        h.compound_style = CompoundStyle::new(Some(accent), None, Attribute::Bold.into());
    }

    skin.bullet = StyledChar::from_fg_char(accent, '•');
    skin.quote_mark = StyledChar::from_fg_char(border_fg, '▎');
    skin.table.compound_style = CompoundStyle::new(Some(border_fg), None, Default::default());
    skin.table_border_chars = STANDARD_TABLE_BORDER_CHARS;

    skin
}

/// Claude Desktop 스타일: 사용자 프롬프트를 둥근 알약/상자 버블(Round Bubble)로 렌더링합니다.
pub fn render_user_prompt_bubble(
    prompt_text: &str,
    image_paths: &[&Path],
    flavor: ThemeFlavor,
    term_width: usize,
) {
    let border = flavor.bubble_border_ansi();
    let muted = flavor.muted_ansi();
    let max_box_width = term_width.saturating_sub(6).clamp(30, 92);

    // 1. 첨부된 이미지 썸네일/배지 칩 (말풍선 상단에 배치)
    if !image_paths.is_empty() {
        print!("  ");
        for (i, p) in image_paths.iter().enumerate() {
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("image");
            print!("{muted}📷 [#{i} {fname}]\x1b[0m ");
        }
        println!();
    }

    let raw_lines: Vec<&str> = prompt_text.lines().collect();
    let mut wrapped_lines = Vec::new();
    for l in raw_lines {
        let trimmed = l.trim_end();
        if trimmed.is_empty() {
            wrapped_lines.push(String::new());
        } else {
            let mut cur = String::new();
            let mut cur_len = 0;
            for ch in trimmed.chars() {
                let w = if unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) > 1 {
                    2
                } else {
                    1
                };
                if cur_len + w > max_box_width - 4 && !cur.is_empty() {
                    wrapped_lines.push(std::mem::take(&mut cur));
                    cur_len = 0;
                }
                cur.push(ch);
                cur_len += w;
            }
            if !cur.is_empty() {
                wrapped_lines.push(cur);
            }
        }
    }

    if wrapped_lines.is_empty() {
        wrapped_lines.push(prompt_text.to_string());
    }

    // 단일 행이면 내용 길이에 딱 맞는 앙증맞은 알약 버블, 여러 행이면 둥근 모서리 상자
    if wrapped_lines.len() == 1 {
        let line = &wrapped_lines[0];
        let line_w = str_display_width(line);
        let bottom_dashes = line_w + 4;
        println!("  {border}╭─\x1b[0m \x1b[1m{line}\x1b[0m {border}─╮\x1b[0m");
        println!("  {border}╰{}╯\x1b[0m", "─".repeat(bottom_dashes));
    } else {
        let content_width = wrapped_lines
            .iter()
            .map(|s| str_display_width(s))
            .max()
            .unwrap_or(4)
            .min(max_box_width - 4);

        println!("  {border}╭{}╮\x1b[0m", "─".repeat(content_width + 2));
        for l in wrapped_lines {
            let l_w = str_display_width(&l);
            let pad = content_width.saturating_sub(l_w);
            println!(
                "  {border}│\x1b[0m \x1b[1m{l}\x1b[0m{:>pad$} {border}│\x1b[0m",
                "",
                pad = pad
            );
        }
        println!("  {border}╰{}╯\x1b[0m", "─".repeat(content_width + 2));
    }
    println!();
    let _ = io::stdout().flush();
}

/// Claude Desktop 스타일: 도구 호출 진행 상황을 접힌 한 줄(Folded Activity Line)로 깔끔하게 렌더링합니다.
pub fn render_folded_tool_activity(
    category_summary: &str,
    is_running: bool,
    elapsed_sec: f64,
    flavor: ThemeFlavor,
) {
    let muted = flavor.muted_ansi();
    let accent = flavor.accent_ansi();
    let symbol = if is_running { "●··" } else { "✓" };
    let time_str = if elapsed_sec > 0.1 {
        format!(" · {:.1}s", elapsed_sec)
    } else {
        String::new()
    };

    // 터미널 현재 줄 덮어쓰기 (\r\x1b[2K)
    print!(
        "\r\x1b[2K  {accent}{symbol}\x1b[0m {category_summary}{muted}{time_str} ›\x1b[0m"
    );
    let _ = io::stdout().flush();
}

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 실시간 진행 상태 표시줄의 단계 종류
pub enum LiveProgressKind<'a> {
    Routing,
    Thinking { completed_tools: Option<&'a str> },
    ToolRunning { label: &'a str },
}

/// stderr 현재 줄에 애니메이션 스피너와 경과 시간을 출력합니다 (stdout 마크다운 버퍼와 간섭하지 않음).
/// 터미널 폭을 초과하여 다음 줄로 넘어가면 \r\x1b[2K가 오작동하므로 절대 줄바꿈이 발생하지 않도록 폭에 맞춥니다.
pub fn render_live_progress_line(
    frame_idx: usize,
    kind: LiveProgressKind<'_>,
    elapsed_sec: f64,
    flavor: ThemeFlavor,
) {
    let frame = SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()];
    let accent = flavor.accent_ansi();
    let muted = flavor.muted_ansi();
    let success = flavor.success_ansi();

    let term_width = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80).max(40);
    let max_text_width = term_width.saturating_sub(14); // 스피너(2칸) + 여백 + 시간 표시 등 여유폭

    match kind {
        LiveProgressKind::Routing => {
            eprint!(
                "\r\x1b[2K  {accent}{frame}\x1b[0m {muted}최적 모델 판별 중... ({elapsed_sec:.1}s)\x1b[0m"
            );
        }
        LiveProgressKind::Thinking {
            completed_tools: Some(summary),
        } => {
            let fitted_summary = truncate_to_display_width(summary, max_text_width.saturating_sub(18));
            eprint!(
                "\r\x1b[2K  {accent}{frame}\x1b[0m {muted}다음 단계 추론 중... ·\x1b[0m {success}✓\x1b[0m {fitted_summary} {muted}({elapsed_sec:.1}s)\x1b[0m"
            );
        }
        LiveProgressKind::Thinking {
            completed_tools: None,
        } => {
            eprint!(
                "\r\x1b[2K  {accent}{frame}\x1b[0m {muted}생각하는 중... ({elapsed_sec:.1}s)\x1b[0m"
            );
        }
        LiveProgressKind::ToolRunning { label } => {
            let fitted_label = truncate_to_display_width(label, max_text_width);
            eprint!(
                "\r\x1b[2K  {accent}{frame}\x1b[0m {fitted_label} {muted}({elapsed_sec:.1}s) ›\x1b[0m"
            );
        }
    }
    let _ = io::stderr().flush();
}

pub fn truncate_to_display_width(s: &str, max_width: usize) -> String {
    let mut cur_width = 0;
    let mut out = String::new();
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if cur_width + w > max_width.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(ch);
        cur_width += w;
    }
    out
}

/// stderr에 그려진 임시 진행 상태 표시줄을 깨끗하게 지웁니다.
pub fn clear_live_progress_line() {
    eprint!("\r\x1b[2K");
    let _ = io::stderr().flush();
}

/// 응답 본문 출력 직전에 누적된 도구 실행 완료 요약을 한 줄로 확정해 남깁니다.
pub fn print_completed_tool_summary_line(summary: &str, elapsed_sec: f64, flavor: ThemeFlavor) {
    if summary.trim().is_empty() {
        return;
    }
    let success = flavor.success_ansi();
    let muted = flavor.muted_ansi();
    let time_str = if elapsed_sec > 0.05 {
        format!(" · {elapsed_sec:.1}s")
    } else {
        String::new()
    };
    println!("  {success}✓\x1b[0m {summary}{muted}{time_str}\x1b[0m");
    let _ = io::stdout().flush();
}

/// Claude Desktop 스타일: 턴 종료 시 고친 파일/통계를 세련된 카드로 렌더링합니다.
pub fn render_turn_summary_card(
    duration_sec: f64,
    input_tokens: u64,
    output_tokens: u64,
    edited_files: &[String],
    flavor: ThemeFlavor,
) {
    let muted = flavor.muted_ansi();
    let accent = flavor.accent_ansi();
    let border = flavor.bubble_border_ansi();

    let in_k = format_token_k(input_tokens);
    let out_k = format_token_k(output_tokens);

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let latest_turn_id = std::fs::read_to_string(Path::new(&runtime_dir).join("agy-clip-latest-turn.id"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let action_links = if let Some(tid) = latest_turn_id {
        let copy_ans = format!("\x1b]8;;agy-clip://code/{tid}\x1b\\[📋 답변 복사]\x1b]8;;\x1b\\");
        let quote_ans = format!("\x1b]8;;agy-clip://quote/{tid}\x1b\\[❝ 답변 인용]\x1b]8;;\x1b\\");
        format!("  {accent}{copy_ans}\x1b[0m {accent}{quote_ans}\x1b[0m")
    } else {
        String::new()
    };

    if !edited_files.is_empty() {
        println!();
        println!(
            "  {border}┌──\x1b[0m {accent}\x1b[1mEdited {} file{}\x1b[0m {muted}({:.1}s · tokens in {in_k} / out {out_k})\x1b[0m{action_links}",
            edited_files.len(),
            if edited_files.len() > 1 { "s" } else { "" },
            duration_sec
        );
        for f in edited_files {
            let fname = Path::new(f)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(f);
            println!("  {border}│\x1b[0m   {accent}•\x1b[0m \x1b[1m{fname}\x1b[0m  {muted}{f}\x1b[0m");
        }
        println!("  {border}└──\x1b[0m");
    } else {
        println!();
        println!(
            "  {muted}── 완료 ({:.1}s · tokens in {in_k} / out {out_k}) ────────────\x1b[0m{action_links}",
            duration_sec
        );
    }
    let _ = io::stdout().flush();
}

fn format_token_k(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

pub struct StreamingMarkdownPrinter {
    flavor: ThemeFlavor,
    skin: MadSkin,
    line_buf: String,
    in_code_block: bool,
    code_lang: String,
    code_buf: String,
    table_buf: String,
    has_printed_anything: bool,
    in_thought_block: bool,
    suppressing_soliloquy_block: bool,
    captured_code_blocks: Vec<CapturedCodeBlock>,
    raw_response: String,
}

#[derive(Debug, Clone)]
pub struct CapturedCodeBlock {
    pub index: usize,
    pub lang: String,
    pub code: String,
    pub id: String,
}

/// 마크다운 원문에서 펜스드 코드 블록(` ``` `)들을 추출하여 번호(`#1`, `#2`...)를 부여합니다.
pub fn extract_code_blocks_from_markdown(markdown: &str) -> Vec<CapturedCodeBlock> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut cur_lang = String::new();
    let mut cur_code = String::new();
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());

    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if in_block {
                in_block = false;
                let idx = blocks.len() + 1;
                let clip_id = format!(
                    "hist_{}_{:x}",
                    idx,
                    SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                );
                let clip_path = Path::new(&runtime_dir).join(format!("agy-clip-{clip_id}.txt"));
                let _ = std::fs::write(&clip_path, &cur_code);
                blocks.push(CapturedCodeBlock {
                    index: idx,
                    lang: std::mem::take(&mut cur_lang),
                    code: std::mem::take(&mut cur_code),
                    id: clip_id,
                });
            } else {
                in_block = true;
                cur_lang = rest.trim().to_string();
                cur_code.clear();
            }
        } else if in_block {
            cur_code.push_str(line);
            cur_code.push('\n');
        }
    }
    blocks
}

impl StreamingMarkdownPrinter {
    pub fn new(flavor: ThemeFlavor) -> Self {
        Self {
            flavor,
            skin: build_skin_for_flavor(flavor),
            line_buf: String::new(),
            in_code_block: false,
            code_lang: String::new(),
            code_buf: String::new(),
            table_buf: String::new(),
            has_printed_anything: false,
            in_thought_block: false,
            suppressing_soliloquy_block: false,
            captured_code_blocks: Vec::new(),
            raw_response: String::new(),
        }
    }

    pub fn push_delta(&mut self, delta: &str) {
        self.raw_response.push_str(delta);
        for ch in delta.chars() {
            if ch == '\n' {
                let completed = std::mem::take(&mut self.line_buf);
                self.process_completed_line(&completed);
            } else if ch != '\r' {
                self.line_buf.push(ch);
            }
        }
    }

    pub fn finish(&mut self) -> (Vec<CapturedCodeBlock>, String) {
        if !self.line_buf.is_empty() {
            let tail = std::mem::take(&mut self.line_buf);
            self.process_completed_line(&tail);
        }
        if self.in_code_block {
            self.in_code_block = false;
            let lang = std::mem::take(&mut self.code_lang);
            let code = std::mem::take(&mut self.code_buf);
            self.render_code_block(&lang, &code);
        }
        self.flush_table_if_any();
        self.suppressing_soliloquy_block = false;
        let _ = io::stdout().flush();
        (self.captured_code_blocks.clone(), self.raw_response.clone())
    }

    pub fn code_blocks(&self) -> &[CapturedCodeBlock] {
        &self.captured_code_blocks
    }

    pub fn raw_response(&self) -> &str {
        &self.raw_response
    }

    fn process_completed_line(&mut self, line: &str) {
        let trimmed = line.trim();

        // 0. 펜스드 코드 블록(` ``` `) 내부 코드는 추론 필터의 영향을 절대 받지 않도록 최우선 처리
        if let Some(rest) = trimmed.strip_prefix("```") {
            self.suppressing_soliloquy_block = false;
            if self.in_code_block {
                self.in_code_block = false;
                let lang = std::mem::take(&mut self.code_lang);
                let code = std::mem::take(&mut self.code_buf);
                self.render_code_block(&lang, &code);
            } else {
                self.flush_table_if_any();
                self.in_code_block = true;
                self.code_lang = rest.trim().to_string();
                self.code_buf.clear();
            }
            return;
        }

        if self.in_code_block {
            self.code_buf.push_str(line);
            self.code_buf.push('\n');
            return;
        }

        // 1. <thought> ... </thought> 및 <thinking> ... </thinking> 태그 제거 (인라인 및 멀티라인)
        let after_inline_tags = strip_inline_thought_tags(line);
        let tag_trimmed = after_inline_tags.trim();

        if self.in_thought_block {
            if let Some(rest) = take_after_closing_thought_tag(&after_inline_tags) {
                self.in_thought_block = false;
                if rest.trim().is_empty() {
                    return;
                }
                self.process_completed_line(rest);
            }
            return;
        }

        if tag_trimmed.starts_with("<thought>") || tag_trimmed.starts_with("<thinking>") {
            self.in_thought_block = true;
            if let Some(rest) = take_after_closing_thought_tag(&after_inline_tags) {
                self.in_thought_block = false;
                if !rest.trim().is_empty() {
                    self.process_completed_line(rest);
                }
            }
            return;
        }

        // 2. 모델 내부 사고/독백 유출 정화 (첫 줄뿐 아니라 도구 호출 중간 턴·인라인 혼합 유출 모두 대응)
        let Some(sanitized_line) =
            sanitize_soliloquy_line(&after_inline_tags, &mut self.suppressing_soliloquy_block)
        else {
            return;
        };
        let sanitized_trimmed = sanitized_line.trim();

        if is_markdown_table_line(sanitized_trimmed) {
            self.table_buf.push_str(&sanitized_line);
            self.table_buf.push('\n');
            return;
        }

        self.flush_table_if_any();

        if sanitized_trimmed.is_empty() {
            if self.has_printed_anything {
                println!();
            }
        } else {
            // 마크다운 링크 [라벨](url)을 터미널 OSC 8 하이퍼링크로 스마트 변환
            let link_enhanced = transform_markdown_links_to_osc8(&sanitized_line);
            let formatted = self.skin.term_text(&link_enhanced).to_string();
            print!("{}", ensure_single_trailing_newline(&formatted));
            let _ = io::stdout().flush();
            self.has_printed_anything = true;
        }
    }

    fn flush_table_if_any(&mut self) {
        if self.table_buf.is_empty() {
            return;
        }
        let raw_table = std::mem::take(&mut self.table_buf);
        let formatted = self.skin.term_text(&raw_table).to_string();
        print!("{}", ensure_single_trailing_newline(&formatted));
        let _ = io::stdout().flush();
        self.has_printed_anything = true;
    }

    fn render_code_block(&mut self, lang: &str, code: &str) {
        let block_idx = self.captured_code_blocks.len() + 1;
        let line_count = code.lines().count();
        let clip_id = format!("{:x}", SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());

        // 로컬 클릭 복사용 임시 파일에 코드 본문 저장 (/tmp/agy-clip-{id}.txt)
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let clip_path = std::path::PathBuf::from(runtime_dir).join(format!("agy-clip-{clip_id}.txt"));
        let _ = std::fs::write(&clip_path, code);

        self.captured_code_blocks.push(CapturedCodeBlock {
            index: block_idx,
            lang: lang.to_string(),
            code: code.to_string(),
            id: clip_id.clone(),
        });

        let accent = self.flavor.accent_ansi();
        let muted = self.flavor.muted_ansi();
        let border = self.flavor.bubble_border_ansi();
        let header_lang = if lang.is_empty() { "code" } else { lang };

        // OSC 8 하이퍼링크: agy-clip://code/{id} 및 agy-clip://quote/{id}
        let copy_link = format!("\x1b]8;;agy-clip://code/{clip_id}\x1b\\[📋 복사]\x1b]8;;\x1b\\");
        let quote_link = format!("\x1b]8;;agy-clip://quote/{clip_id}\x1b\\[❝ 인용]\x1b]8;;\x1b\\");

        println!(
            "{border}  ┌──\x1b[0m {accent}\x1b[1m#{block_idx}\x1b[0m {muted}{header_lang} ({line_count} lines)\x1b[0m  {accent}{copy_link}\x1b[0m {accent}{quote_link}\x1b[0m {muted}(/c {block_idx} · /quote {block_idx})\x1b[0m"
        );

        if let Some(highlighted) = try_highlight_with_bat(lang, code, self.flavor) {
            for hl_line in highlighted.lines() {
                // 좌측 테두리선 없이 4칸 공백 인덴트로 출력하여 마우스 드래그 시 테두리 기호가 긁히지 않음
                println!("    {hl_line}");
            }
        } else {
            let fallback_code_ansi = match self.flavor {
                ThemeFlavor::Light => "\x1b[38;5;23m",
                ThemeFlavor::Dark => "\x1b[38;5;152m",
            };
            for raw_line in code.lines() {
                println!("    {fallback_code_ansi}{raw_line}\x1b[0m");
            }
        }
        println!("{border}  └──\x1b[0m");
        let _ = io::stdout().flush();
        self.has_printed_anything = true;
    }
}

/// `[텍스트](url)` 링크를 터미널 OSC 8 하이퍼링크(`\x1b]8;;url\x1b\텍스트\x1b]8;;\x1b\`)로 변환합니다.
pub fn transform_markdown_links_to_osc8(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remainder = text;

    while let Some(start_bracket) = remainder.find('[') {
        result.push_str(&remainder[..start_bracket]);
        let after_bracket = &remainder[start_bracket + 1..];

        if let Some(end_bracket) = after_bracket.find(']') {
            let label = &after_bracket[..end_bracket];
            let after_label = &after_bracket[end_bracket + 1..];

            if after_label.starts_with('(')
                && let Some(end_paren) = after_label.find(')')
            {
                let raw_url = &after_label[1..end_paren];
                if raw_url.starts_with("http://")
                    || raw_url.starts_with("https://")
                    || raw_url.starts_with("file://")
                {
                    // 터미널 OSC 8 하이퍼링크 문법
                    result.push_str(&format!("\x1b]8;;{raw_url}\x1b\\{label}\x1b]8;;\x1b\\"));
                    remainder = &after_label[end_paren + 1..];
                    continue;
                }
            }
            result.push('[');
            result.push_str(label);
            result.push(']');
            remainder = after_label;
            continue;
        }

        result.push('[');
        remainder = after_bracket;
    }

    result.push_str(remainder);
    result
}

pub fn str_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

fn is_markdown_table_line(trimmed: &str) -> bool {
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 3
}

/// 모델이 응답 첫머리나 중간에 출력할 수 있는 내부 사고/독백 줄인지 감지합니다.
pub fn is_soliloquy_thought_line(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    starts_with_soliloquy_marker(trimmed)
}

const SOLILOQUY_MARKERS_EXACT: &[&str] = &[
    "잠깐!!",
    "잠깐! ",
    "잠깐, ",
    "잠깐만!!",
    "잠깐만! ",
    "1초만!!",
    "1초만! ",
    "1초만,",
    "1초만 ",
    "생각해 보자",
    "생각해보자",
    "어디 보자",
    "สักครู่",
];

const SOLILOQUY_MARKERS_ASCII_LOWER: &[&str] = &[
    "wait:",
    "wait,",
    "wait...",
    "wait —",
    "wait -",
    "wait a second",
    "hold on!",
    "hold on,",
    "let's call ",
    "let's read ",
    "let's run ",
    "let's check ",
    "let's use ",
    "let's see ",
    "let's inspect ",
    "let's look ",
    "let's examine ",
    "let's search ",
    "let's verify ",
    "let's think",
    "let me think",
    "let me check",
    "hmm, let's",
    "i'm thinking",
];

fn starts_with_soliloquy_marker(s: &str) -> bool {
    let trimmed = s.trim_start();
    if SOLILOQUY_MARKERS_EXACT
        .iter()
        .any(|m| trimmed.starts_with(m))
        || trimmed == "잠깐!"
        || trimmed == "잠깐만!"
        || trimmed == "1초만"
        || trimmed == "1초만!"
    {
        return true;
    }
    let lower_prefix: String = trimmed.chars().take(24).collect::<String>().to_lowercase();
    SOLILOQUY_MARKERS_ASCII_LOWER
        .iter()
        .any(|m| lower_prefix.starts_with(m))
}

fn is_multiline_soliloquy_opener(trimmed: &str) -> bool {
    let lower: String = trimmed.chars().take(24).collect::<String>().to_lowercase();
    trimmed.starts_with("잠깐!!")
        || trimmed.starts_with("1초만!!")
        || lower.starts_with("wait:")
        || lower.starts_with("wait a second")
        || lower.starts_with("hold on!")
}

fn contains_hangul(s: &str) -> bool {
    s.chars().any(is_hangul_syllable)
}

fn is_hangul_syllable(c: char) -> bool {
    ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

fn strip_inline_thought_tags(line: &str) -> String {
    let mut cur = line.to_string();
    for (open_tag, close_tag) in [("<thought>", "</thought>"), ("<thinking>", "</thinking>")] {
        while let Some(start) = cur.find(open_tag) {
            if let Some(rel_end) = cur[start + open_tag.len()..].find(close_tag) {
                let end = start + open_tag.len() + rel_end + close_tag.len();
                cur.replace_range(start..end, "");
            } else {
                break;
            }
        }
    }
    cur
}

fn take_after_closing_thought_tag(line: &str) -> Option<&str> {
    if let Some(idx) = line.find("</thought>") {
        Some(&line[idx + "</thought>".len()..])
    } else {
        line.find("</thinking>")
            .map(|idx| &line[idx + "</thinking>".len()..])
    }
}

/// 문장 시작 또는 종결 부호(`.`, `!`, `?`) 직후에 시작하는 인라인 독백 마커의 바이트 인덱스를 찾습니다.
fn find_first_soliloquy_marker_index(line: &str) -> Option<usize> {
    let lead_ws = line.len() - line.trim_start().len();
    if starts_with_soliloquy_marker(&line[lead_ws..]) {
        return Some(lead_ws);
    }

    for (idx, ch) in line.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let after_punct = idx + ch.len_utf8();
            let candidate = &line[after_punct..];
            let ws_len = candidate.len() - candidate.trim_start().len();
            let start_idx = after_punct + ws_len;
            if start_idx < line.len() && starts_with_soliloquy_marker(&line[start_idx..]) {
                return Some(start_idx);
            }
        }
    }
    None
}

/// 독백 구간 뒤쪽에 정상적인 한국어 안내 문장(또는 `[파일](...)` 링크로 시작하는 한국어 문장)이 이어지면 복원합니다.
fn extract_trailing_korean_sentence(soliloquy_and_rest: &str) -> Option<&str> {
    for (idx, ch) in soliloquy_and_rest.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let after_punct = idx + ch.len_utf8();
            let rest = soliloquy_and_rest[after_punct..].trim_start();
            if rest.is_empty() || starts_with_soliloquy_marker(rest) {
                continue;
            }
            let first = rest.chars().next()?;
            if is_hangul_syllable(first) {
                return Some(rest);
            }
            // `[src/lib.rs](file:///...)와 ... 확인하고 있습니다.` 형태 지원
            if first == '['
                && let Some(close_paren) = rest.find(')')
            {
                let after_link = rest[close_paren + 1..].trim_start();
                if after_link.chars().next().is_some_and(is_hangul_syllable) {
                    return Some(rest);
                }
            }
        }
    }
    None
}

/// 한 줄에서 내부 독백/자문자답 유출 구간만 제거하고 사용자용 텍스트만 남깁니다.
pub fn sanitize_soliloquy_line(line: &str, suppressing_block: &mut bool) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        if *suppressing_block {
            *suppressing_block = false;
            return None;
        }
        return Some(String::new());
    }

    if let Some(marker_idx) = find_first_soliloquy_marker_index(line) {
        let prefix = line[..marker_idx].trim_end();
        let suffix = extract_trailing_korean_sentence(&line[marker_idx..]).unwrap_or("");

        match (!prefix.is_empty(), !suffix.is_empty()) {
            (true, true) => {
                *suppressing_block = false;
                return Some(format!("{prefix} {suffix}"));
            }
            (true, false) => {
                *suppressing_block = false;
                return Some(prefix.to_string());
            }
            (false, true) => {
                *suppressing_block = false;
                return Some(suffix.to_string());
            }
            (false, false) => {
                if is_multiline_soliloquy_opener(trimmed) {
                    *suppressing_block = true;
                }
                return None;
            }
        }
    }

    if *suppressing_block {
        // 직전 줄이 `Wait: ...` 같은 다중 행 추론 블록 오프너였던 경우, 한국어 본문이 나오기 전까지 영문 추론 줄 억제
        if contains_hangul(trimmed) {
            *suppressing_block = false;
            return Some(line.to_string());
        }
        return None;
    }

    Some(line.to_string())
}

/// 멀티라인 마크다운 텍스트에서 코드 블록은 보존한 채 유출된 `<thought>` 태그와 내부 독백만 정화합니다.
pub fn sanitize_leaked_thoughts_for_display(markdown: &str) -> String {
    let mut out_lines = Vec::new();
    let mut in_code = false;
    let mut in_thought = false;
    let mut suppressing = false;

    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") {
            suppressing = false;
            in_code = !in_code;
            out_lines.push(raw_line.to_string());
            continue;
        }
        if in_code {
            out_lines.push(raw_line.to_string());
            continue;
        }

        let stripped = strip_inline_thought_tags(raw_line);
        if in_thought {
            if let Some(rest) = take_after_closing_thought_tag(&stripped) {
                in_thought = false;
                if !rest.trim().is_empty()
                    && let Some(clean) = sanitize_soliloquy_line(rest, &mut suppressing)
                {
                    out_lines.push(clean);
                }
            }
            continue;
        }
        if stripped.trim_start().starts_with("<thought>")
            || stripped.trim_start().starts_with("<thinking>")
        {
            in_thought = true;
            if let Some(rest) = take_after_closing_thought_tag(&stripped) {
                in_thought = false;
                if !rest.trim().is_empty()
                    && let Some(clean) = sanitize_soliloquy_line(rest, &mut suppressing)
                {
                    out_lines.push(clean);
                }
            }
            continue;
        }

        if let Some(clean) = sanitize_soliloquy_line(&stripped, &mut suppressing) {
            out_lines.push(clean);
        }
    }

    out_lines.join("\n")
}

fn ensure_single_trailing_newline(s: &str) -> String {
    let trimmed_end = s.trim_end_matches('\n');
    format!("{trimmed_end}\n")
}

fn try_highlight_with_bat(lang: &str, code: &str, flavor: ThemeFlavor) -> Option<String> {
    let mut cmd = Command::new("bat");
    cmd.arg("--color=always")
        .arg("--style=plain")
        .arg("--paging=never");

    if std::env::var_os("BAT_THEME").is_none() {
        cmd.arg(format!("--theme={}", flavor.default_bat_theme()));
    }

    if !lang.is_empty()
        && lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '+' || c == '_')
    {
        cmd.arg("-l").arg(lang);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(code.as_bytes()).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_markdown_file_and_http_links_to_osc8() {
        let input = "참고: [agy_route.rs](file:///workspace/src/code.rs) 및 [구글](https://google.com)";
        let transformed = transform_markdown_links_to_osc8(input);
        assert!(transformed.contains("\x1b]8;;file:///workspace/src/code.rs\x1b\\agy_route.rs\x1b]8;;\x1b\\"));
        assert!(transformed.contains("\x1b]8;;https://google.com\x1b\\구글\x1b]8;;\x1b\\"));
    }

    #[test]
    fn leaves_regular_brackets_intact() {
        let input = "테스트 [일반 괄호] 입니다";
        assert_eq!(transform_markdown_links_to_osc8(input), input);
    }

    #[test]
    fn truncates_long_label_to_terminal_width_safely() {
        let long_cmd = "export PATH=\"$HOME/.cargo/bin:$HOME/.local/share/mise/shims:$PATH\" && cargo test && cargo build --release";
        let truncated = truncate_to_display_width(long_cmd, 30);
        assert!(str_display_width(&truncated) <= 30);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn single_line_bubble_width_matches_top_and_bottom() {
        for word in ["데헷", "a", "12345", "Cargo.toml 첫 줄만 읽어줘"] {
            let line_w = str_display_width(word);
            let top_len = 1 + 1 + 1 + line_w + 1 + 1 + 1; // ╭ + ─ + ' ' + word + ' ' + ─ + ╮
            let bottom_dashes = line_w + 4;
            let bottom_len = 1 + bottom_dashes + 1; // ╰ + ─*bottom_dashes + ╯
            assert_eq!(top_len, bottom_len, "word: {word}");
        }
    }

    #[test]
    fn strips_korean_and_mixed_inline_soliloquy_without_losing_user_message() {
        // 1. 단독 한국어 독백 줄 제거
        let raw1 = "잠깐!! 1초만!! 생각해보자...\n정답은 42입니다.";
        assert_eq!(sanitize_leaked_thoughts_for_display(raw1), "정답은 42입니다.");

        // 2. 실제 트랜스크립트 유출 패턴 1: 한글 문장 + 태국어/영어 인라인 독백 + 후행 한글 중계문
        let raw2 = "현재 작업 중인 프로젝트 구조를 확인해보겠습니다.สักครู่! Let's call `run_command` or `list_dir`? Wait: we don't have `list_dir` in declarations. Let's check the directory contents with `run_command`.현재 프로젝트와 관련 저장소 구성을 확인하고 있습니다.";
        assert_eq!(
            sanitize_leaked_thoughts_for_display(raw2),
            "현재 작업 중인 프로젝트 구조를 확인해보겠습니다. 현재 프로젝트와 관련 저장소 구성을 확인하고 있습니다."
        );

        // 3. 실제 트랜스크립트 유출 패턴 2: 선행 영어 독백 + 마크다운 링크로 시작하는 한글 중계문
        let raw3 = "Let's read `src/lib.rs` and `src/bin/agy_route.rs`![src/lib.rs](file:///tmp/lib.rs)와 [agy_route.rs](file:///tmp/agy_route.rs)를 읽어 구성을 확인하고 있습니다.";
        assert_eq!(
            sanitize_leaked_thoughts_for_display(raw3),
            "[src/lib.rs](file:///tmp/lib.rs)와 [agy_route.rs](file:///tmp/agy_route.rs)를 읽어 구성을 확인하고 있습니다."
        );
    }

    #[test]
    fn suppresses_multiline_english_soliloquy_block_and_preserves_code_blocks() {
        let raw = "\
Wait: look at the conversation history carefully!
Why did the user ask this?
1. First hypothesis in internal monologue.
Hold on! Let's check `src/render.rs`.
코드 구조를 확인하고 수정 사항을 정리했습니다:
```rust
// 잠깐!! 코드 블록 안의 주석과 문자열은 절대 지워지면 안 됩니다.
let msg = \"Wait: do not strip inside code blocks!\";
```";
        let cleaned = sanitize_leaked_thoughts_for_display(raw);
        assert!(!cleaned.contains("Wait: look at"));
        assert!(!cleaned.contains("Hold on!"));
        assert!(cleaned.contains("코드 구조를 확인하고 수정 사항을 정리했습니다:"));
        assert!(cleaned.contains("잠깐!! 코드 블록 안의 주석과 문자열은 절대 지워지면 안 됩니다."));
        assert!(cleaned.contains("let msg = \"Wait: do not strip inside code blocks!\";"));
    }
}
