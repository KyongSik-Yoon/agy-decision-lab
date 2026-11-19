use std::{
    collections::VecDeque,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STD};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

const MAX_REPOS: usize = 16;
const DEFAULT_RECENT_COMMITS: usize = 5;
const MAX_RECENT_COMMITS: usize = 20;
const DEFAULT_MAX_PATCH_CHARS: usize = 12_000;
const MAX_PATCH_CHARS_LIMIT: usize = 40_000;
const MAX_CHANGED_FILES_PER_REPO: usize = 100;

const MAX_SLICES: usize = 24;
const MAX_SEARCHES: usize = 12;
const MAX_SLICE_LINES: usize = 400;
const DEFAULT_CONTEXT_LINES: usize = 2;
const MAX_CONTEXT_LINES: usize = 10;
const DEFAULT_MAX_MATCHES: usize = 30;
const MAX_MATCHES_LIMIT: usize = 100;
const DEFAULT_TOTAL_CHARS: usize = 32_000;
const MAX_TOTAL_CHARS_LIMIT: usize = 80_000;

const SENSITIVE_EXCLUDE_GLOBS: &[&str] = &[
    "!.env",
    "!.env.*",
    "!*.env",
    "!**/.ssh/**",
    "!.ssh/**",
    "!**/.aws/**",
    "!.aws/**",
    "!**/.kube/**",
    "!.kube/**",
    "!**/.gnupg/**",
    "!.gnupg/**",
    "!**/.docker/**",
    "!.docker/**",
    "!*.pem",
    "!*.key",
    "!*.p12",
    "!*.pfx",
    "!*.kdbx",
    "!*.jks",
    "!*.keystore",
    "!*id_rsa*",
    "!*id_ed25519*",
    "!*id_dsa*",
    "!*id_ecdsa*",
    "!credentials.json",
    "!*credentials.json",
    "!.git-credentials",
    "!.pgpass",
    "!hosts.yml",
    "!kubeconfig",
    "!.npmrc",
    "!.netrc",
    "!.pypirc",
];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceGitDigestArgs {
    /// 조회할 Git 저장소 경로 목록 (생략하거나 비어 있으면 현재 작업 디렉터리 "." 사용, 최대 16개)
    pub repo_paths: Option<Vec<String>>,
    /// 저장소별 최근 커밋 로그 조회 개수 (기본값: 5, 최대 20)
    pub recent_commits: Option<usize>,
    /// 변경분의 unified diff 발췌 포함 여부 (기본값: false)
    pub include_diff_patch: Option<bool>,
    /// 저장소당 diff 발췌 최대 글자 수 (기본값: 12000, 최대 40000)
    pub max_patch_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileSliceSpec {
    /// 읽어올 파일 경로 (절대 경로 또는 현재 작업 디렉터리 기준 상대 경로)
    pub path: String,
    /// 시작 줄 번호 (1-based, tail_lines 미지정 시 기본값 1)
    pub start_line: Option<usize>,
    /// 종료 줄 번호 (1-based inclusive, 구간당 최대 400줄로 제한)
    pub end_line: Option<usize>,
    /// 지정 시 파일 끝에서 N줄만 추출 (대용량 로그 확인용, 최대 400줄)
    pub tail_lines: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PatternSearchSpec {
    /// 검색 대상 파일 또는 디렉터리 경로
    pub root_path: String,
    /// 검색할 정규식 또는 고정 문자열 패턴
    pub pattern: String,
    /// ripgrep glob 필터 (예: "*.rs", "!target/**")
    pub glob: Option<String>,
    /// 고정 리터럴 문자열 검색 여부 (기본값: false = 정규식)
    pub fixed_strings: Option<bool>,
    /// 매치 앞뒤 문맥 줄 수 (기본값: 2, 최대 10)
    pub context_lines: Option<usize>,
    /// 파일/경로당 최대 매치 수 (기본값: 30, 최대 100)
    pub max_matches: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchCodeContextArgs {
    /// 일괄 추출할 파일 라인 구간 또는 tail 목록 (최대 24개)
    pub slices: Option<Vec<FileSliceSpec>>,
    /// 일괄 실행할 코드/로그 패턴 검색 목록 (최대 12개)
    pub searches: Option<Vec<PatternSearchSpec>>,
    /// 응답 내 추출 본문 총 글자 수 상한 (기본값: 32000, 최대 80000)
    pub max_total_chars: Option<usize>,
}

#[derive(Debug, Serialize)]
struct GitChangedFile {
    xy: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct GitRepoDigest {
    repo_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ahead: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    behind: Option<i64>,
    staged_count: usize,
    unstaged_count: usize,
    untracked_count: usize,
    changed_files: Vec<GitChangedFile>,
    changed_files_truncated: bool,
    recent_commits: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_stat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    staged_diff_stat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_patch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SliceResult {
    path: String,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_lines: Option<usize>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    root_path: String,
    pattern: String,
    engine: String,
    matched_lines: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 다중 Git 저장소의 상태, 최근 커밋, diff 요약을 병렬로 수집합니다.
pub async fn run_workspace_git_digest(args: WorkspaceGitDigestArgs) -> Result<serde_json::Value> {
    let raw_paths = args
        .repo_paths
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let repo_paths = if raw_paths.is_empty() {
        vec![".".to_string()]
    } else {
        raw_paths.into_iter().take(MAX_REPOS).collect()
    };

    let recent_commits = args
        .recent_commits
        .unwrap_or(DEFAULT_RECENT_COMMITS)
        .clamp(1, MAX_RECENT_COMMITS);
    let include_diff_patch = args.include_diff_patch.unwrap_or(false);
    let max_patch_chars = args
        .max_patch_chars
        .unwrap_or(DEFAULT_MAX_PATCH_CHARS)
        .clamp(500, MAX_PATCH_CHARS_LIMIT);

    let mut handles = Vec::with_capacity(repo_paths.len());
    for repo_path in repo_paths {
        handles.push(tokio::spawn(async move {
            collect_single_repo_digest(
                repo_path,
                recent_commits,
                include_diff_patch,
                max_patch_chars,
            )
            .await
        }));
    }

    let mut repos = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(digest) => repos.push(digest),
            Err(err) => repos.push(GitRepoDigest {
                repo_path: String::new(),
                branch: None,
                oid: None,
                upstream: None,
                ahead: None,
                behind: None,
                staged_count: 0,
                unstaged_count: 0,
                untracked_count: 0,
                changed_files: Vec::new(),
                changed_files_truncated: false,
                recent_commits: Vec::new(),
                diff_stat: None,
                staged_diff_stat: None,
                diff_patch: None,
                patch_truncated: None,
                error: Some(format!("내부 작업 실행 오류: {err}")),
            }),
        }
    }

    Ok(serde_json::json!({
        "status": "ok",
        "repo_count": repos.len(),
        "repos": repos,
    }))
}

async fn run_git_command(repo_path: &str, git_args: &[&str]) -> Result<(bool, String, String)> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks").arg("-C").arg(repo_path);
    for arg in git_args {
        cmd.arg(arg);
    }
    let output = cmd
        .output()
        .await
        .with_context(|| format!("git 실행 실패 ({repo_path})"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((output.status.success(), stdout, stderr))
}

async fn collect_single_repo_digest(
    repo_path: String,
    recent_commits: usize,
    include_diff_patch: bool,
    max_patch_chars: usize,
) -> GitRepoDigest {
    let mut digest = GitRepoDigest {
        repo_path: repo_path.clone(),
        branch: None,
        oid: None,
        upstream: None,
        ahead: None,
        behind: None,
        staged_count: 0,
        unstaged_count: 0,
        untracked_count: 0,
        changed_files: Vec::new(),
        changed_files_truncated: false,
        recent_commits: Vec::new(),
        diff_stat: None,
        staged_diff_stat: None,
        diff_patch: None,
        patch_truncated: None,
        error: None,
    };

    let status_res = run_git_command(&repo_path, &["status", "--porcelain=v2", "--branch"]).await;
    match status_res {
        Ok((true, stdout, _)) => parse_porcelain_v2(&stdout, &mut digest),
        Ok((false, _, stderr)) => {
            digest.error = Some(stderr.trim().to_string());
            return digest;
        }
        Err(err) => {
            digest.error = Some(format!("{err:#}"));
            return digest;
        }
    }

    let log_limit = recent_commits.to_string();
    let log_args = ["log", "-n", &log_limit, "--oneline", "--decorate"];
    let (log_res, diff_stat_res, cached_stat_res) = tokio::join!(
        run_git_command(&repo_path, &log_args),
        run_git_command(&repo_path, &["diff", "--stat"]),
        run_git_command(&repo_path, &["diff", "--cached", "--stat"]),
    );

    if let Ok((true, stdout, _)) = log_res {
        digest.recent_commits = stdout
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
    }
    if let Ok((true, stdout, _)) = diff_stat_res {
        let trimmed = stdout.trim();
        if !trimmed.is_empty() {
            digest.diff_stat = Some(trimmed.to_string());
        }
    }
    if let Ok((true, stdout, _)) = cached_stat_res {
        let trimmed = stdout.trim();
        if !trimmed.is_empty() {
            digest.staged_diff_stat = Some(trimmed.to_string());
        }
    }

    if include_diff_patch {
        let raw_patch = match run_git_command(&repo_path, &["diff", "HEAD"]).await {
            Ok((true, stdout, _)) => stdout,
            _ => {
                let (wt_res, st_res) = tokio::join!(
                    run_git_command(&repo_path, &["diff"]),
                    run_git_command(&repo_path, &["diff", "--cached"]),
                );
                let mut combined = String::new();
                if let Ok((true, st, _)) = st_res
                    && !st.trim().is_empty()
                {
                    combined.push_str("# staged\n");
                    combined.push_str(&st);
                }
                if let Ok((true, wt, _)) = wt_res
                    && !wt.trim().is_empty()
                {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("# unstaged\n");
                    combined.push_str(&wt);
                }
                combined
            }
        };

        let (sliced, truncated) = truncate_chars(&raw_patch, max_patch_chars);
        digest.diff_patch = Some(sliced);
        digest.patch_truncated = Some(truncated);
    }

    digest
}

fn parse_porcelain_v2(stdout: &str, digest: &mut GitRepoDigest) {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            digest.oid = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.head ") {
            digest.branch = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            digest.upstream = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(a) = part.strip_prefix('+') {
                    digest.ahead = a.parse::<i64>().ok();
                } else if let Some(b) = part.strip_prefix('-') {
                    digest.behind = b.parse::<i64>().ok();
                }
            }
        } else if let Some(rest) = line.strip_prefix("? ") {
            digest.untracked_count += 1;
            let path = rest.trim_end_matches(&['\r', '\n'][..]);
            push_changed_file(digest, "??", path);
        } else if line.starts_with("1 ") {
            let parts: Vec<&str> = line.splitn(9, ' ').collect();
            if parts.len() == 9 {
                record_tracked_entry(digest, parts[1], parts[8]);
            }
        } else if line.starts_with("2 ") {
            let parts: Vec<&str> = line.splitn(10, ' ').collect();
            if parts.len() == 10 {
                let path = match parts[9].split_once('\t') {
                    Some((new_path, orig_path)) => format!("{orig_path} -> {new_path}"),
                    None => parts[9].to_string(),
                };
                record_tracked_entry(digest, parts[1], &path);
            }
        } else if line.starts_with("u ") {
            let parts: Vec<&str> = line.splitn(11, ' ').collect();
            if parts.len() == 11 {
                record_tracked_entry(digest, parts[1], parts[10]);
            }
        }
    }
}

fn record_tracked_entry(digest: &mut GitRepoDigest, xy: &str, path: &str) {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    if x != '.' {
        digest.staged_count += 1;
    }
    if y != '.' {
        digest.unstaged_count += 1;
    }
    push_changed_file(digest, xy, path);
}

fn push_changed_file(digest: &mut GitRepoDigest, xy: &str, path: &str) {
    if digest.changed_files.len() < MAX_CHANGED_FILES_PER_REPO {
        digest.changed_files.push(GitChangedFile {
            xy: xy.to_string(),
            path: path.to_string(),
        });
    } else {
        digest.changed_files_truncated = true;
    }
}

/// 여러 파일 구간/꼬리 및 패턴 검색을 한 번에 수집하고 민감 파일 필터와 총 글자 수 예산을 적용합니다.
pub async fn run_batch_code_context(args: BatchCodeContextArgs) -> Result<serde_json::Value> {
    let max_total_chars = args
        .max_total_chars
        .unwrap_or(DEFAULT_TOTAL_CHARS)
        .clamp(1_000, MAX_TOTAL_CHARS_LIMIT);

    let slice_specs: Vec<FileSliceSpec> = args
        .slices
        .unwrap_or_default()
        .into_iter()
        .take(MAX_SLICES)
        .collect();
    let search_specs: Vec<PatternSearchSpec> = args
        .searches
        .unwrap_or_default()
        .into_iter()
        .take(MAX_SEARCHES)
        .collect();

    let mut slice_handles = Vec::with_capacity(slice_specs.len());
    for spec in slice_specs {
        slice_handles.push(tokio::spawn(async move { collect_file_slice(spec).await }));
    }

    let mut search_handles = Vec::with_capacity(search_specs.len());
    for spec in search_specs {
        search_handles.push(tokio::spawn(async move { collect_pattern_search(spec).await }));
    }

    let mut remaining_chars = max_total_chars;
    let mut slices = Vec::with_capacity(slice_handles.len());
    for h in slice_handles {
        let mut res = match h.await {
            Ok(r) => r,
            Err(err) => SliceResult {
                path: String::new(),
                mode: "error".to_string(),
                start_line: None,
                end_line: None,
                total_lines: None,
                truncated: false,
                content: None,
                error: Some(format!("내부 슬라이스 읽기 실패: {err}")),
            },
        };
        apply_char_budget(&mut res.content, &mut res.truncated, &mut remaining_chars);
        slices.push(res);
    }

    let mut searches = Vec::with_capacity(search_handles.len());
    for h in search_handles {
        let mut res = match h.await {
            Ok(r) => r,
            Err(err) => SearchResult {
                root_path: String::new(),
                pattern: String::new(),
                engine: "error".to_string(),
                matched_lines: 0,
                truncated: false,
                output: None,
                error: Some(format!("내부 패턴 검색 실패: {err}")),
            },
        };
        apply_char_budget(&mut res.output, &mut res.truncated, &mut remaining_chars);
        searches.push(res);
    }

    Ok(serde_json::json!({
        "status": "ok",
        "budget": {
            "max_total_chars": max_total_chars,
            "remaining_chars": remaining_chars,
        },
        "slices": slices,
        "searches": searches,
    }))
}

fn apply_char_budget(text_opt: &mut Option<String>, truncated: &mut bool, remaining: &mut usize) {
    let Some(text) = text_opt.as_mut() else {
        return;
    };
    let char_len = text.chars().count();
    if *remaining == 0 {
        if !text.is_empty() {
            *text = "[총 응답 크기 예산 초과로 생략됨]".to_string();
            *truncated = true;
        }
        return;
    }
    if char_len > *remaining {
        let (sliced, _) = truncate_chars(text, *remaining);
        *text = sliced;
        *truncated = true;
        *remaining = 0;
    } else {
        *remaining -= char_len;
    }
}

async fn collect_file_slice(spec: FileSliceSpec) -> SliceResult {
    let path_str = spec.path.trim().to_string();
    if is_sensitive_or_symlink_target(Path::new(&path_str)) {
        return SliceResult {
            path: path_str,
            mode: "blocked".to_string(),
            start_line: None,
            end_line: None,
            total_lines: None,
            truncated: false,
            content: None,
            error: Some("민감 시크릿 경로는 열람할 수 없습니다".to_string()),
        };
    }

    let file = match tokio::fs::File::open(&path_str).await {
        Ok(f) => f,
        Err(err) => {
            return SliceResult {
                path: path_str,
                mode: "error".to_string(),
                start_line: None,
                end_line: None,
                total_lines: None,
                truncated: false,
                content: None,
                error: Some(format!("파일 열기 실패: {err}")),
            };
        }
    };

    let mut reader = BufReader::new(file);
    let mut buf = Vec::with_capacity(512);

    if let Some(raw_tail) = spec.tail_lines {
        let tail_count = raw_tail.clamp(1, MAX_SLICE_LINES);
        let mut ring: VecDeque<(usize, String)> = VecDeque::with_capacity(tail_count);
        let mut total_lines = 0usize;

        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    total_lines += 1;
                    let line = trim_line_ending(String::from_utf8_lossy(&buf).as_ref());
                    if ring.len() == tail_count {
                        ring.pop_front();
                    }
                    ring.push_back((total_lines, line));
                }
                Err(err) => {
                    return SliceResult {
                        path: path_str,
                        mode: "tail".to_string(),
                        start_line: None,
                        end_line: None,
                        total_lines: Some(total_lines),
                        truncated: false,
                        content: None,
                        error: Some(format!("파일 읽기 실패: {err}")),
                    };
                }
            }
        }

        let start_line = ring.front().map(|(n, _)| *n);
        let end_line = ring.back().map(|(n, _)| *n);
        let formatted = ring
            .into_iter()
            .map(|(n, l)| format!("L{n}: {l}"))
            .collect::<Vec<_>>()
            .join("\n");

        return SliceResult {
            path: path_str,
            mode: "tail".to_string(),
            start_line,
            end_line,
            total_lines: Some(total_lines),
            truncated: total_lines > tail_count || raw_tail > MAX_SLICE_LINES,
            content: Some(formatted),
            error: None,
        };
    }

    let start_line = spec.start_line.unwrap_or(1).max(1);
    let requested_end = spec.end_line.unwrap_or(start_line + MAX_SLICE_LINES - 1);
    let effective_end = if requested_end < start_line {
        start_line
    } else {
        requested_end.min(start_line + MAX_SLICE_LINES - 1)
    };
    let clamped_by_window = spec.end_line.is_none_or(|e| e > effective_end);

    let mut lines = Vec::new();
    let mut line_no = 0usize;
    let mut hit_eof = false;

    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => {
                hit_eof = true;
                break;
            }
            Ok(_) => {
                line_no += 1;
                if line_no < start_line {
                    continue;
                }
                if line_no > effective_end {
                    break;
                }
                let line = trim_line_ending(String::from_utf8_lossy(&buf).as_ref());
                lines.push(format!("L{line_no}: {line}"));
            }
            Err(err) => {
                return SliceResult {
                    path: path_str,
                    mode: "range".to_string(),
                    start_line: Some(start_line),
                    end_line: Some(effective_end),
                    total_lines: None,
                    truncated: false,
                    content: None,
                    error: Some(format!("파일 읽기 실패: {err}")),
                };
            }
        }
    }

    let actual_end = if lines.is_empty() {
        None
    } else {
        Some(start_line + lines.len() - 1)
    };

    SliceResult {
        path: path_str,
        mode: "range".to_string(),
        start_line: Some(start_line),
        end_line: actual_end,
        total_lines: if hit_eof { Some(line_no) } else { None },
        truncated: !hit_eof && clamped_by_window,
        content: Some(lines.join("\n")),
        error: None,
    }
}

async fn collect_pattern_search(spec: PatternSearchSpec) -> SearchResult {
    let root_path = spec.root_path.trim().to_string();
    let pattern = spec.pattern;

    if is_sensitive_or_symlink_target(Path::new(&root_path)) {
        return SearchResult {
            root_path,
            pattern,
            engine: "blocked".to_string(),
            matched_lines: 0,
            truncated: false,
            output: None,
            error: Some("민감 시크릿 경로는 검색할 수 없습니다".to_string()),
        };
    }

    if pattern.is_empty() {
        return SearchResult {
            root_path,
            pattern,
            engine: "invalid".to_string(),
            matched_lines: 0,
            truncated: false,
            output: None,
            error: Some("검색 패턴이 비어 있습니다".to_string()),
        };
    }

    let context_lines = spec
        .context_lines
        .unwrap_or(DEFAULT_CONTEXT_LINES)
        .min(MAX_CONTEXT_LINES);
    let max_matches = spec
        .max_matches
        .unwrap_or(DEFAULT_MAX_MATCHES)
        .clamp(1, MAX_MATCHES_LIMIT);
    let fixed_strings = spec.fixed_strings.unwrap_or(false);
    let fetch_limit = max_matches.saturating_add(1);

    let mut cmd = Command::new("rg");
    cmd.arg("--json")
        .arg("--glob-case-insensitive")
        .arg("--max-count")
        .arg(fetch_limit.to_string())
        .arg("-C")
        .arg(context_lines.to_string());

    if fixed_strings {
        cmd.arg("-F");
    }
    if let Some(glob) = spec.glob.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        cmd.arg("-g").arg(glob);
    }
    for excl in SENSITIVE_EXCLUDE_GLOBS {
        cmd.arg("-g").arg(excl);
    }
    cmd.arg("--").arg(&pattern).arg(&root_path);

    match cmd.output().await {
        Ok(output) => {
            let code = output.status.code().unwrap_or(-1);
            if code == 0 || code == 1 {
                let raw_stdout = String::from_utf8_lossy(&output.stdout);
                let (formatted, matched_lines, truncated) =
                    parse_rg_json_output(&raw_stdout, max_matches, context_lines);
                SearchResult {
                    root_path,
                    pattern,
                    engine: "rg".to_string(),
                    matched_lines,
                    truncated,
                    output: Some(formatted),
                    error: None,
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                SearchResult {
                    root_path,
                    pattern,
                    engine: "rg".to_string(),
                    matched_lines: 0,
                    truncated: false,
                    output: None,
                    error: Some(if stderr.is_empty() {
                        format!("rg 종료 코드: {code}")
                    } else {
                        stderr
                    }),
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fallback_literal_search(&root_path, &pattern, context_lines, max_matches).await
        }
        Err(err) => SearchResult {
            root_path,
            pattern,
            engine: "rg".to_string(),
            matched_lines: 0,
            truncated: false,
            output: None,
            error: Some(format!("rg 실행 실패: {err}")),
        },
    }
}

struct RgFileEvent {
    line_no: usize,
    is_match: bool,
    text: String,
}

fn decode_rg_field(field: &serde_json::Value) -> String {
    if let Some(s) = field.get("text").and_then(|v| v.as_str()) {
        return trim_line_ending(s);
    }
    if let Some(b64) = field.get("bytes").and_then(|v| v.as_str())
        && let Ok(bytes) = BASE64_STD.decode(b64)
    {
        return trim_line_ending(&String::from_utf8_lossy(&bytes));
    }
    String::new()
}

#[allow(clippy::too_many_arguments)]
fn flush_rg_file_events(
    file_path: &str,
    events: &[RgFileEvent],
    max_matches: usize,
    context_lines: usize,
    out_lines: &mut Vec<String>,
    total_matched_lines: &mut usize,
    any_truncated: &mut bool,
    prev_emitted: &mut Option<(String, usize)>,
) {
    if events.is_empty() || file_path.is_empty() || is_sensitive_or_symlink_target(Path::new(file_path))
    {
        return;
    }

    let kept_match_lines: Vec<usize> = events
        .iter()
        .filter(|e| e.is_match)
        .take(max_matches)
        .map(|e| e.line_no)
        .collect();

    if kept_match_lines.is_empty() {
        return;
    }

    let total_file_matches = events.iter().filter(|e| e.is_match).count();
    let file_truncated = total_file_matches > max_matches;
    if file_truncated {
        *any_truncated = true;
    }

    *total_matched_lines += kept_match_lines.len();

    let mut emitted_matches = 0usize;
    for ev in events {
        if ev.is_match {
            if emitted_matches >= max_matches {
                // 초과 매치 지점에 도달하면 후속 문맥·매치 출력은 즉시 중단
                break;
            }
            emitted_matches += 1;
        } else if file_truncated
            && !kept_match_lines
                .iter()
                .any(|&m| ev.line_no.abs_diff(m) <= context_lines)
        {
            continue;
        }

        if let Some((prev_path, prev_line)) = prev_emitted.as_ref()
            && (prev_path != file_path || ev.line_no > prev_line.saturating_add(1))
        {
            out_lines.push("--".to_string());
        }

        let sep = if ev.is_match { ':' } else { '-' };
        out_lines.push(format!("{file_path}{sep}{}{sep}{}", ev.line_no, ev.text));
        *prev_emitted = Some((file_path.to_string(), ev.line_no));
    }
}

fn parse_rg_json_output(
    stdout: &str,
    max_matches: usize,
    context_lines: usize,
) -> (String, usize, bool) {
    let mut out_lines = Vec::new();
    let mut total_matched_lines = 0usize;
    let mut truncated = false;
    let mut prev_emitted: Option<(String, usize)> = None;

    let mut current_path = String::new();
    let mut current_events: Vec<RgFileEvent> = Vec::new();

    for raw_line in stdout.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(kind) = val.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(data) = val.get("data") else {
            continue;
        };

        match kind {
            "begin" => {
                if !current_events.is_empty() {
                    flush_rg_file_events(
                        &current_path,
                        &current_events,
                        max_matches,
                        context_lines,
                        &mut out_lines,
                        &mut total_matched_lines,
                        &mut truncated,
                        &mut prev_emitted,
                    );
                    current_events.clear();
                }
                current_path = data.get("path").map(decode_rg_field).unwrap_or_default();
            }
            "match" | "context" => {
                let path = data
                    .get("path")
                    .map(decode_rg_field)
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| current_path.clone());
                if current_path.is_empty() {
                    current_path = path.clone();
                } else if path != current_path {
                    flush_rg_file_events(
                        &current_path,
                        &current_events,
                        max_matches,
                        context_lines,
                        &mut out_lines,
                        &mut total_matched_lines,
                        &mut truncated,
                        &mut prev_emitted,
                    );
                    current_events.clear();
                    current_path = path;
                }
                let line_no = data
                    .get("line_number")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let text = data.get("lines").map(decode_rg_field).unwrap_or_default();
                current_events.push(RgFileEvent {
                    line_no,
                    is_match: kind == "match",
                    text,
                });
            }
            "end" => {
                flush_rg_file_events(
                    &current_path,
                    &current_events,
                    max_matches,
                    context_lines,
                    &mut out_lines,
                    &mut total_matched_lines,
                    &mut truncated,
                    &mut prev_emitted,
                );
                current_events.clear();
                current_path.clear();
            }
            _ => {}
        }
    }

    if !current_events.is_empty() {
        flush_rg_file_events(
            &current_path,
            &current_events,
            max_matches,
            context_lines,
            &mut out_lines,
            &mut total_matched_lines,
            &mut truncated,
            &mut prev_emitted,
        );
    }

    (out_lines.join("\n"), total_matched_lines, truncated)
}

async fn fallback_literal_search(
    root_path: &str,
    pattern: &str,
    context_lines: usize,
    max_matches: usize,
) -> SearchResult {
    let root = PathBuf::from(root_path);
    let pat = pattern.to_string();
    let res = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        collect_walk_files(&root, &mut files)?;

        let mut out_lines = Vec::new();
        let mut total_hits = 0usize;
        let mut truncated = false;

        'outer: for file_path in files {
            let Ok(bytes) = std::fs::read(&file_path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let mut file_hits = 0usize;
            for (idx, line) in lines.iter().enumerate() {
                if line.contains(&pat) {
                    file_hits += 1;
                    total_hits += 1;
                    let start = idx.saturating_sub(context_lines);
                    let end = (idx + context_lines).min(lines.len().saturating_sub(1));
                    for (c_idx, c_line) in lines.iter().enumerate().take(end + 1).skip(start) {
                        let sep = if c_idx == idx { ':' } else { '-' };
                        out_lines.push(format!(
                            "{}{sep}{}{sep}{c_line}",
                            file_path.display(),
                            c_idx + 1
                        ));
                    }
                    if file_hits >= max_matches || total_hits >= max_matches * 4 {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }
        Ok::<_, std::io::Error>((out_lines.join("\n"), total_hits, truncated))
    })
    .await;

    match res {
        Ok(Ok((output, matched_lines, truncated))) => SearchResult {
            root_path: root_path.to_string(),
            pattern: pattern.to_string(),
            engine: "rust_literal_fallback".to_string(),
            matched_lines,
            truncated,
            output: Some(output),
            error: None,
        },
        Ok(Err(err)) => SearchResult {
            root_path: root_path.to_string(),
            pattern: pattern.to_string(),
            engine: "rust_literal_fallback".to_string(),
            matched_lines: 0,
            truncated: false,
            output: None,
            error: Some(format!("폴백 검색 실패: {err}")),
        },
        Err(err) => SearchResult {
            root_path: root_path.to_string(),
            pattern: pattern.to_string(),
            engine: "rust_literal_fallback".to_string(),
            matched_lines: 0,
            truncated: false,
            output: None,
            error: Some(format!("폴백 태스크 오류: {err}")),
        },
    }
}

fn collect_walk_files(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if is_sensitive_or_symlink_target(path) {
        return Ok(());
    }
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let child = entry.path();
            if let Some(name) = child.file_name().and_then(|n| n.to_str())
                && matches!(name, ".git" | "target" | "node_modules")
            {
                continue;
            }
            if file_type.is_dir() {
                collect_walk_files(&child, out)?;
            } else if file_type.is_file() && !is_sensitive_or_symlink_target(&child) {
                out.push(child);
            }
        }
    }
    Ok(())
}

/// 시크릿 경로 자체 또는 심볼릭 링크가 가리키는 대상이 시크릿 경로인지 확인합니다.
pub fn is_sensitive_or_symlink_target(path: &Path) -> bool {
    if is_sensitive_path(path) {
        return true;
    }
    if let Ok(link_target) = std::fs::read_link(path)
        && is_sensitive_path(&link_target)
    {
        return true;
    }
    if let Ok(canon) = std::fs::canonicalize(path)
        && is_sensitive_path(&canon)
    {
        return true;
    }
    false
}

/// 시크릿·개인키·자격증명 파일 경로를 차단합니다 (예제 파일 `.env.example` 등은 허용).
pub fn is_sensitive_path(path: &Path) -> bool {
    for comp in path.components() {
        if let Component::Normal(os_str) = comp {
            let s = os_str.to_string_lossy().to_ascii_lowercase();
            if matches!(s.as_str(), ".ssh" | ".aws" | ".kube" | ".gnupg" | ".docker") {
                return true;
            }
        }
    }

    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = file_name.to_ascii_lowercase();

    if matches!(
        lower.as_str(),
        ".env.example" | ".env.sample" | ".env.template"
    ) || lower.ends_with(".env.example")
        || lower.ends_with(".env.sample")
        || lower.ends_with(".env.template")
    {
        return false;
    }

    if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") {
        return true;
    }

    if matches!(
        lower.as_str(),
        "credentials.json"
            | ".npmrc"
            | ".netrc"
            | ".pypirc"
            | ".git-credentials"
            | ".pgpass"
            | "hosts.yml"
            | "kubeconfig"
            | "shadow"
            | "gshadow"
    ) {
        return true;
    }

    if ["id_rsa", "id_ed25519", "id_dsa", "id_ecdsa"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }

    [".pem", ".key", ".p12", ".pfx", ".kdbx", ".jks", ".keystore"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn trim_line_ending(raw: &str) -> String {
    raw.trim_end_matches(&['\r', '\n'][..]).to_string()
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => (text[..byte_idx].to_string(), true),
        None => (text.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "agy_ws_test_{prefix}_{}_{}",
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn git_digest_reads_manifest_repo() {
        let res = run_workspace_git_digest(WorkspaceGitDigestArgs {
            repo_paths: Some(vec![env!("CARGO_MANIFEST_DIR").to_string()]),
            recent_commits: Some(3),
            include_diff_patch: Some(false),
            max_patch_chars: None,
        })
        .await
        .expect("workspace_git_digest must succeed");

        assert_eq!(res["status"], "ok");
        assert_eq!(res["repo_count"], 1);
        let repo = &res["repos"][0];
        assert!(repo["error"].is_null());
        assert!(repo["branch"].as_str().is_some_and(|b| !b.is_empty()));
        assert!(!repo["recent_commits"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn batch_context_reads_slices_searches_and_blocks_secrets() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cargo_toml = root.join("Cargo.toml").display().to_string();
        let src_dir = root.join("src").display().to_string();
        let secret_env = root.join(".env").display().to_string();
        let example_env = root.join(".env.example").display().to_string();

        let res = run_batch_code_context(BatchCodeContextArgs {
            slices: Some(vec![
                FileSliceSpec {
                    path: cargo_toml.clone(),
                    start_line: Some(1),
                    end_line: Some(5),
                    tail_lines: None,
                },
                FileSliceSpec {
                    path: cargo_toml,
                    start_line: None,
                    end_line: None,
                    tail_lines: Some(3),
                },
                FileSliceSpec {
                    path: secret_env,
                    start_line: Some(1),
                    end_line: Some(5),
                    tail_lines: None,
                },
                FileSliceSpec {
                    path: example_env,
                    start_line: Some(1),
                    end_line: Some(3),
                    tail_lines: None,
                },
            ]),
            searches: Some(vec![PatternSearchSpec {
                root_path: src_dir,
                pattern: "DecisionMcpServer".to_string(),
                glob: Some("*.rs".to_string()),
                fixed_strings: Some(true),
                context_lines: Some(1),
                max_matches: Some(10),
            }]),
            max_total_chars: Some(20_000),
        })
        .await
        .expect("batch_code_context must succeed");

        assert_eq!(res["status"], "ok");
        let slices = res["slices"].as_array().unwrap();
        assert_eq!(slices.len(), 4);
        assert!(slices[0]["content"]
            .as_str()
            .unwrap()
            .contains("agy-decision-lab"));
        assert_eq!(slices[1]["mode"], "tail");
        assert_eq!(slices[2]["mode"], "blocked");
        assert!(slices[2]["error"].as_str().is_some());
        assert_eq!(slices[3]["mode"], "range");
        assert!(slices[3]["error"].is_null());

        let searches = res["searches"].as_array().unwrap();
        assert_eq!(searches.len(), 1);
        assert!(searches[0]["matched_lines"].as_u64().unwrap() > 0);
    }

    #[test]
    fn sensitive_path_filter_cases() {
        assert!(is_sensitive_path(Path::new("/tmp/project/.env")));
        assert!(is_sensitive_path(Path::new("/tmp/project/.env.local")));
        assert!(is_sensitive_path(Path::new("/home/user/.config/agy-decision-lab/config.env")));
        assert!(is_sensitive_path(Path::new("/tmp/project/prod.env")));
        assert!(is_sensitive_path(Path::new("/home/user/.ssh/id_ed25519")));
        assert!(is_sensitive_path(Path::new("/home/user/.aws/credentials")));
        assert!(is_sensitive_path(Path::new("/home/user/.kube/config")));
        assert!(is_sensitive_path(Path::new("/home/user/.gnupg/secring.gpg")));
        assert!(is_sensitive_path(Path::new("/home/user/.git-credentials")));
        assert!(is_sensitive_path(Path::new("/tmp/hosts.yml")));
        assert!(is_sensitive_path(Path::new("/tmp/server.key")));
        assert!(is_sensitive_path(Path::new("/tmp/credentials.json")));
        assert!(!is_sensitive_path(Path::new("/tmp/project/.env.example")));
        assert!(!is_sensitive_path(Path::new("/tmp/project/example.env.example")));
        assert!(!is_sensitive_path(Path::new("/tmp/project/Cargo.toml")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_to_secret_is_blocked_in_slice_and_search() {
        let tmp = TempDirGuard::new("symlink");
        let secret_file = tmp.0.join("config.env");
        std::fs::write(&secret_file, "GEMINI_API_KEY=secret_value_123\n").unwrap();
        let safe_link = tmp.0.join("safe_link.txt");
        std::os::unix::fs::symlink(&secret_file, &safe_link).unwrap();

        let res = run_batch_code_context(BatchCodeContextArgs {
            slices: Some(vec![FileSliceSpec {
                path: safe_link.display().to_string(),
                start_line: Some(1),
                end_line: Some(5),
                tail_lines: None,
            }]),
            searches: Some(vec![
                PatternSearchSpec {
                    root_path: safe_link.display().to_string(),
                    pattern: "secret_value_123".to_string(),
                    glob: None,
                    fixed_strings: Some(true),
                    context_lines: Some(1),
                    max_matches: Some(10),
                },
                PatternSearchSpec {
                    root_path: tmp.0.display().to_string(),
                    pattern: "secret_value_123".to_string(),
                    glob: None,
                    fixed_strings: Some(true),
                    context_lines: Some(1),
                    max_matches: Some(10),
                },
            ]),
            max_total_chars: Some(10_000),
        })
        .await
        .expect("batch_code_context must succeed");

        assert_eq!(res["slices"][0]["mode"], "blocked");
        assert_eq!(res["searches"][0]["engine"], "blocked");
        assert_eq!(res["searches"][1]["matched_lines"], 0);
        assert_eq!(res["searches"][1]["output"], "");
    }

    #[tokio::test]
    async fn rg_search_counts_exact_matches_and_sets_truncated_on_overflow() {
        let tmp = TempDirGuard::new("rg_counts");
        let sample = tmp.0.join("sample.txt");
        let body = [
            "line 1 alpha",
            "line 2 beta",
            "line 3 NEEDLE one",
            "line 4 gamma",
            "line 5 delta",
            "line 6 epsilon",
            "line 7 zeta",
            "line 8 NEEDLE two",
            "line 9 eta",
            "line 10 theta",
        ]
        .join("\n");
        std::fs::write(&sample, body).unwrap();

        let res_all = run_batch_code_context(BatchCodeContextArgs {
            slices: None,
            searches: Some(vec![
                PatternSearchSpec {
                    root_path: sample.display().to_string(),
                    pattern: "NEEDLE".to_string(),
                    glob: None,
                    fixed_strings: Some(true),
                    context_lines: Some(2),
                    max_matches: Some(10),
                },
                PatternSearchSpec {
                    root_path: sample.display().to_string(),
                    pattern: "NEEDLE".to_string(),
                    glob: None,
                    fixed_strings: Some(true),
                    context_lines: Some(2),
                    max_matches: Some(1),
                },
            ]),
            max_total_chars: Some(10_000),
        })
        .await
        .expect("batch_code_context must succeed");

        let s0 = &res_all["searches"][0];
        assert_eq!(s0["engine"], "rg");
        assert_eq!(s0["matched_lines"], 2);
        assert_eq!(s0["truncated"], false);
        assert!(s0["output"].as_str().unwrap().contains(":3:line 3 NEEDLE one"));
        assert!(s0["output"].as_str().unwrap().contains(":8:line 8 NEEDLE two"));

        let s1 = &res_all["searches"][1];
        assert_eq!(s1["engine"], "rg");
        assert_eq!(s1["matched_lines"], 1);
        assert_eq!(s1["truncated"], true);
        let out1 = s1["output"].as_str().unwrap();
        assert!(out1.contains(":3:line 3 NEEDLE one"));
        assert!(!out1.contains("NEEDLE two"));
        assert!(!out1.contains("line 6 epsilon"));
    }

    #[test]
    fn parse_porcelain_v2_preserves_spaces_and_record_types() {
        let stdout = "\
# branch.oid 0123456789abcdef
# branch.head main
# branch.upstream origin/main
# branch.ab +1 -2
1 .M N... 100644 100644 100644 111 222 docs/my guide.md
2 R. N... 100644 100644 100644 333 444 R100 docs/new name.md\tdocs/old name.md
u UU N... 100644 100644 100644 100644 111 222 333 conflict dir/file name.txt
? spaced file.txt
";
        let mut digest = GitRepoDigest {
            repo_path: ".".to_string(),
            branch: None,
            oid: None,
            upstream: None,
            ahead: None,
            behind: None,
            staged_count: 0,
            unstaged_count: 0,
            untracked_count: 0,
            changed_files: Vec::new(),
            changed_files_truncated: false,
            recent_commits: Vec::new(),
            diff_stat: None,
            staged_diff_stat: None,
            diff_patch: None,
            patch_truncated: None,
            error: None,
        };

        parse_porcelain_v2(stdout, &mut digest);

        assert_eq!(digest.branch.as_deref(), Some("main"));
        assert_eq!(digest.ahead, Some(1));
        assert_eq!(digest.behind, Some(2));
        assert_eq!(digest.staged_count, 2);
        assert_eq!(digest.unstaged_count, 2);
        assert_eq!(digest.untracked_count, 1);
        assert_eq!(digest.changed_files.len(), 4);
        assert_eq!(digest.changed_files[0].xy, ".M");
        assert_eq!(digest.changed_files[0].path, "docs/my guide.md");
        assert_eq!(digest.changed_files[1].xy, "R.");
        assert_eq!(
            digest.changed_files[1].path,
            "docs/old name.md -> docs/new name.md"
        );
        assert_eq!(digest.changed_files[2].xy, "UU");
        assert_eq!(
            digest.changed_files[2].path,
            "conflict dir/file name.txt"
        );
        assert_eq!(digest.changed_files[3].xy, "??");
        assert_eq!(digest.changed_files[3].path, "spaced file.txt");
    }
}
