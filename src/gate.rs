use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    client::DecisionClient,
    router::env_or,
    types::{
        Answer, DEFAULT_DECISION_MODEL, DEFAULT_DECISIONS_ENDPOINT, DecisionInput, DecisionRequest,
        NoulCriteria, Question,
    },
};

const MAX_FAST_PATH_TOTAL_LEN: usize = 600;
const MAX_FAST_PATH_SEGMENT_LEN: usize = 240;
const MAX_FAST_PATH_CHAIN_SEGMENTS: usize = 5;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliSettingsRoots {
    #[serde(default)]
    trusted_workspaces: Vec<String>,
}

static SAFE_TRUSTED_ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateEvaluation {
    /// "allow" | "ask" | "force_ask" | "deny"
    pub action: String,
    pub fast_path: bool,
    pub reason: String,
    pub risk_score: f64,
    pub risk_confidence: f64,
    pub destructive_probability: f64,
    pub secret_exposure_probability: f64,
    pub latency_ms: u128,
}

fn current_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn is_safe_workspace_root(path: &Path, home_dir: Option<&Path>) -> bool {
    if !path.is_absolute() {
        return false;
    }

    let mut normal_count = 0usize;
    let mut first_normal: Option<&str> = None;
    for comp in path.components() {
        match comp {
            Component::RootDir => {}
            Component::Normal(seg) => {
                normal_count += 1;
                if first_normal.is_none() {
                    first_normal = seg.to_str();
                }
            }
            Component::ParentDir | Component::CurDir | Component::Prefix(_) => return false,
        }
    }

    // '/', '/etc', '/home', '/home/<user>' 같은 광범위 루트 차단
    if normal_count < 3 {
        return false;
    }

    if matches!(
        first_normal,
        Some("etc" | "var" | "usr" | "root" | "tmp" | "proc" | "sys" | "dev" | "run")
    ) {
        return false;
    }

    if let Some(home) = home_dir {
        if path == home || path == home.join("Downloads") {
            return false;
        }
        if let Ok(rel) = path.strip_prefix(home) {
            let mut rel_comps = rel.components();
            let first_rel = rel_comps.next().and_then(|c| c.as_os_str().to_str());
            let second_rel = rel_comps.next().and_then(|c| c.as_os_str().to_str());
            if let Some(seg1) = first_rel
                && seg1.starts_with('.')
            {
                let is_worktree_root =
                    matches!(seg1, ".herdr" | ".paseo") && second_rel == Some("worktrees");
                if !is_worktree_root {
                    return false;
                }
            }
        }
    }

    true
}

fn filter_safe_trusted_roots<I, S>(entries: I, home_dir: Option<&Path>) -> Vec<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut roots = Vec::new();
    for entry in entries {
        let trimmed = entry.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        if is_safe_workspace_root(&path, home_dir) && !roots.contains(&path) {
            roots.push(path);
        }
    }
    roots
}

fn load_safe_trusted_roots() -> &'static [PathBuf] {
    SAFE_TRUSTED_ROOTS.get_or_init(|| {
        let Some(home) = current_home_dir() else {
            return Vec::new();
        };
        let settings_path = home.join(".gemini/antigravity-cli/settings.json");
        let Ok(meta) = fs::symlink_metadata(&settings_path) else {
            return Vec::new();
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Vec::new();
        }
        let Ok(raw) = fs::read_to_string(&settings_path) else {
            return Vec::new();
        };
        let Ok(parsed) = serde_json::from_str::<CliSettingsRoots>(&raw) else {
            return Vec::new();
        };
        filter_safe_trusted_roots(parsed.trusted_workspaces, Some(&home))
    })
}

fn is_unsafe_path_value(val: &str, safe_cwd: Option<&Path>, trusted_roots: &[PathBuf]) -> bool {
    if val.is_empty() {
        return false;
    }

    if val == ".."
        || val.starts_with("../")
        || val.ends_with("/..")
        || val.contains("/../")
    {
        return true;
    }

    let path = Path::new(val);
    for comp in path.components() {
        match comp {
            Component::ParentDir => return true,
            Component::Normal(seg)
                if matches!(
                    seg.to_str(),
                    Some(".config" | ".local" | ".gemini" | ".mozilla" | ".pki")
                ) =>
            {
                return true;
            }
            _ => {}
        }
    }

    if !val.starts_with('/') {
        return false;
    }

    if let Some(cwd_path) = safe_cwd
        && path.starts_with(cwd_path)
    {
        return false;
    }

    !trusted_roots.iter().any(|root| path.starts_with(root))
}

fn is_unsafe_path_arg(arg: &str, safe_cwd: Option<&Path>, trusted_roots: &[PathBuf]) -> bool {
    let unquoted = arg.trim_matches(|c| c == '\'' || c == '"');
    let val = if unquoted.starts_with('-') {
        unquoted.split_once('=').map(|(_, v)| v).unwrap_or("")
    } else {
        unquoted
    };
    is_unsafe_path_value(val, safe_cwd, trusted_roots)
}

fn has_unsafe_path_args(
    args: &[&str],
    safe_cwd: Option<&Path>,
    trusted_roots: &[PathBuf],
) -> bool {
    args.iter()
        .any(|arg| is_unsafe_path_arg(arg, safe_cwd, trusted_roots))
}

fn has_even_quotes(seg: &str) -> bool {
    let singles = seg.chars().filter(|&c| c == '\'').count();
    let doubles = seg.chars().filter(|&c| c == '"').count();
    singles.is_multiple_of(2) && doubles.is_multiple_of(2)
}

fn is_single_zero_ms_safe_command(
    segment: &str,
    safe_cwd: Option<&Path>,
    trusted_roots: &[PathBuf],
) -> bool {
    let parts: Vec<&str> = segment.split_whitespace().collect();
    let Some(&first) = parts.first() else {
        return false;
    };
    let bin = first.rsplit('/').next().unwrap_or(first);

    match bin {
        "pwd" | "whoami" | "uname" | "date" | "uptime" | "df" | "free" | "ls" | "ll" | "tree"
        | "wc" | "stat" | "file" | "which" | "type" | "realpath" | "dirname" | "basename" => true,
        "cat" | "head" | "tail" | "bat" | "less" | "rg" | "grep" | "fd" | "jq" => {
            !has_unsafe_path_args(&parts[1..], safe_cwd, trusted_roots)
        }
        "find" => {
            !has_unsafe_path_args(&parts[1..], safe_cwd, trusted_roots)
                && !parts
                    .iter()
                    .any(|p| matches!(*p, "-delete" | "-exec" | "-execdir" | "-ok" | "-fprint"))
        }
        "git" => is_safe_git_command(&parts[1..], safe_cwd, trusted_roots),
        "cargo" => {
            let sub_ok = matches!(
                parts.get(1).copied(),
                Some(
                    "check"
                        | "clippy"
                        | "fmt"
                        | "test"
                        | "build"
                        | "metadata"
                        | "tree"
                        | "--version"
                        | "-V"
                )
            );
            sub_ok
                && !parts[1..]
                    .iter()
                    .any(|p| *p == "--config" || p.starts_with("--config="))
                && !has_unsafe_path_args(&parts[1..], safe_cwd, trusted_roots)
        }
        "go" => is_safe_go_command(&parts[1..], safe_cwd, trusted_roots),
        "rustc" | "rustup" => parts.get(1).copied() == Some("--version"),
        _ => false,
    }
}

fn is_safe_git_command(args: &[&str], safe_cwd: Option<&Path>, trusted_roots: &[PathBuf]) -> bool {
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx];
        if arg == "-C" {
            let Some(dir) = args.get(idx + 1).copied() else {
                return false;
            };
            let unquoted = dir.trim_matches(|c| c == '\'' || c == '"');
            if unquoted.starts_with('-')
                || is_unsafe_path_value(unquoted, safe_cwd, trusted_roots)
            {
                return false;
            }
            idx += 2;
        } else if let Some(dir) = arg.strip_prefix("-C") {
            let unquoted = dir.trim_matches(|c| c == '\'' || c == '"');
            if unquoted.is_empty()
                || unquoted.starts_with('-')
                || is_unsafe_path_value(unquoted, safe_cwd, trusted_roots)
            {
                return false;
            }
            idx += 1;
        } else if matches!(arg, "--no-pager" | "--no-optional-locks") {
            idx += 1;
        } else {
            break;
        }
    }

    let Some(&sub) = args.get(idx) else {
        return false;
    };
    let rest = &args[idx + 1..];

    match sub {
        "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "blame" => {
            let has_output_or_ext = rest.iter().any(|p| {
                *p == "-o"
                    || *p == "--output"
                    || p.starts_with("--output=")
                    || *p == "--ext-diff"
            });
            !has_output_or_ext && !has_unsafe_path_args(rest, safe_cwd, trusted_roots)
        }
        "branch" => {
            let has_mutating_flag = rest.iter().any(|p| {
                matches!(
                    *p,
                    "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--move" | "--copy"
                        | "-f" | "--force"
                )
            });
            !has_mutating_flag && !has_unsafe_path_args(rest, safe_cwd, trusted_roots)
        }
        _ => false,
    }
}

fn is_safe_go_command(args: &[&str], safe_cwd: Option<&Path>, trusted_roots: &[PathBuf]) -> bool {
    let Some(&sub) = args.first() else {
        return false;
    };
    let rest = &args[1..];
    match sub {
        "version" => rest.is_empty(),
        "vet" | "test" | "build" | "list" => {
            let has_risky_flag = rest.iter().any(|p| {
                *p == "-exec"
                    || p.starts_with("-exec=")
                    || *p == "-toolexec"
                    || p.starts_with("-toolexec=")
                    || *p == "-o"
                    || p.starts_with("-o=")
            });
            !has_risky_flag && !has_unsafe_path_args(rest, safe_cwd, trusted_roots)
        }
        "env" => {
            let has_mutating_flag = rest
                .iter()
                .any(|p| *p == "-w" || *p == "-u" || p.starts_with("-w=") || p.starts_with("-u="));
            !has_mutating_flag && !has_unsafe_path_args(rest, safe_cwd, trusted_roots)
        }
        _ => false,
    }
}

fn is_zero_ms_safe_command_with_roots(
    cmd: &str,
    cwd: Option<&str>,
    trusted_roots: &[PathBuf],
    home_dir: Option<&Path>,
) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_FAST_PATH_TOTAL_LEN {
        return false;
    }

    // 파이프, 세미콜론, 리다이렉션, 명령 치환, 글롭, 변수 확장, 홈 확장, 이스케이프 포함 시 정밀 채점으로 넘김
    let forbidden_chars = [
        ';', '|', '>', '<', '`', '\n', '\r', '*', '?', '[', ']', '$', '~', '\\', '{', '}',
    ];
    if trimmed.chars().any(|c| forbidden_chars.contains(&c)) {
        return false;
    }

    // 민감 경로·시크릿 키워드·키 프리픽스가 인자에 포함되면 정밀 채점으로 넘김
    let lower = trimmed.to_ascii_lowercase();
    let sensitive_tokens = [
        ".env",
        ".ssh",
        ".aws",
        ".kube",
        ".gnupg",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".docker",
        ".git-credentials",
        ".pgpass",
        "kubeconfig",
        "hosts.yml",
        "config.env",
        "id_rsa",
        "id_ed25519",
        "shadow",
        "keepass",
        "secret",
        "token",
        "credential",
        "apikey",
        "api_key",
        "private_key",
        "private key",
        "aiza",
        "ghp_",
        "glpat-",
        "xoxb-",
        "sk-",
        "/etc/passwd",
        "/etc/gshadow",
        "/proc/",
    ];
    if sensitive_tokens.iter().any(|tok| lower.contains(tok)) {
        return false;
    }

    let safe_cwd_buf = cwd
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| is_safe_workspace_root(p, home_dir));
    let safe_cwd = safe_cwd_buf.as_deref();

    let raw_segments: Vec<&str> = trimmed.split("&&").collect();
    if raw_segments.is_empty() || raw_segments.len() > MAX_FAST_PATH_CHAIN_SEGMENTS {
        return false;
    }

    for raw_seg in raw_segments {
        let seg = raw_seg.trim();
        if seg.is_empty()
            || seg.len() > MAX_FAST_PATH_SEGMENT_LEN
            || seg.contains('&')
            || !has_even_quotes(seg)
        {
            return false;
        }
        if !is_single_zero_ms_safe_command(seg, safe_cwd, trusted_roots) {
            return false;
        }
    }

    true
}

/// 셸 메타문자·리다이렉션·민감 파일 접근이 없는 순수 조회/로컬 검증 명령(단일 및 `&&` 체인)인지 0ms로 판별합니다.
pub fn is_zero_ms_safe_command(cmd: &str) -> bool {
    is_zero_ms_safe_command_with_cwd(cmd, None)
}

/// 현재 작업 디렉터리(`cwd`)와 검증된 신뢰 워크스페이스 루트를 함께 고려해 0ms 로컬 패스트패스 여부를 판별합니다.
pub fn is_zero_ms_safe_command_with_cwd(cmd: &str, cwd: Option<&str>) -> bool {
    let home = current_home_dir();
    let trusted_roots = load_safe_trusted_roots();
    is_zero_ms_safe_command_with_roots(cmd, cwd, trusted_roots, home.as_deref())
}

fn check_protected_credential_store(command: &str) -> Option<GateEvaluation> {
    let normalized = command.to_ascii_lowercase();
    let trimmed = normalized.trim_start();

    if trimmed.starts_with("agy-decision-hook status") {
        return None;
    }

    let without_rules = normalized.replace(".config/agy-decision-lab/rules", "");
    let touches_non_rules_config = without_rules.contains(".config/agy-decision-lab");

    let protected_patterns = [
        "config.env",
        ".ssh/id_",
        ".aws/credentials",
        ".netrc",
        ".git-credentials",
        "/etc/shadow",
        "/etc/gshadow",
    ];

    if touches_non_rules_config || protected_patterns.iter().any(|p| normalized.contains(p)) {
        return Some(GateEvaluation {
            action: "force_ask".to_string(),
            fast_path: true,
            reason: "민감 인증·자격 증명 저장소 접근 감지 (로컬 강제 확인)".to_string(),
            risk_score: 2.5,
            risk_confidence: 1.0,
            destructive_probability: 0.0,
            secret_exposure_probability: 1.0,
            latency_ms: 0,
        });
    }

    None
}

pub async fn evaluate_command_for_hook(
    command: &str,
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<GateEvaluation> {
    if let Some(blocked) = check_protected_credential_store(command) {
        return Ok(blocked);
    }

    if is_zero_ms_safe_command_with_cwd(command, cwd) {
        return Ok(GateEvaluation {
            action: "allow".to_string(),
            fast_path: true,
            reason: "단순 읽기 전용/검증 명령 로컬 패스트패스 허용 (0ms)".to_string(),
            risk_score: 0.0,
            risk_confidence: 1.0,
            destructive_probability: 0.0,
            secret_exposure_probability: 0.0,
            latency_ms: 0,
        });
    }

    let _ = crate::runtime_config::load_runtime_env();
    let api_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty() && v != "your_ai_studio_api_key_here")
        .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY 미설정"))?;

    let endpoint = env_or("GEMINI_DECISIONS_ENDPOINT", DEFAULT_DECISIONS_ENDPOINT);
    let model = env_or("GEMINI_DECISION_MODEL", DEFAULT_DECISION_MODEL);
    let client = DecisionClient::new(api_key, endpoint, model.clone(), timeout)?;

    let combined_input = match cwd {
        Some(dir) if !dir.trim().is_empty() => {
            format!("Working directory: {dir}\nCandidate shell command:\n{command}")
        }
        _ => format!("Candidate shell command:\n{command}"),
    };

    let mut questions = BTreeMap::new();
    questions.insert(
        "risk_score".to_string(),
        Question::Score {
            instructions:
                "Rate the operational and security blast radius of executing this shell command."
                    .to_string(),
            criteria: vec![
                "Read-only inspection or harmless local query with zero side effects.".to_string(),
                "Scoped reversible local workspace build, test, or file edit.".to_string(),
                "External network mutation, git push, package installation, or service restart."
                    .to_string(),
                "Irreversible data/file deletion, history wipe, privilege escalation, or credential exfiltration."
                    .to_string(),
            ],
        },
    );
    questions.insert(
        "is_destructive".to_string(),
        Question::Noul {
            instructions: "Can this command irreversibly delete, overwrite, or corrupt files, git history, databases, or system state?".to_string(),
            criteria: Some(NoulCriteria {
                true_description: "Uses recursive/force deletion, hard reset, force push, disk/table wipe, or destructive overwrite.".to_string(),
                false_description: "Read-only inspection, normal build/test, or reversible workspace operation.".to_string(),
            }),
        },
    );
    questions.insert(
        "exposes_secrets".to_string(),
        Question::Noul {
            instructions: "Does this command read, print, or transmit secret keys, tokens, .env credentials, or private authentication files over stdout or network?".to_string(),
            criteria: Some(NoulCriteria {
                true_description: "Reads, dumps, greps, or transmits .env/config.env files, SSH/GPG/cloud/Kubernetes credentials, auth tokens, API keys, or wildcard dumps of ~/.config, ~/.ssh, ~/.aws, or secret directories.".to_string(),
                false_description: "Inspects ordinary project source code, build outputs, tests, or non-sensitive workspace files.".to_string(),
            }),
        },
    );

    let req = DecisionRequest::new(DecisionInput::Text(combined_input), questions).with_model(model);
    let outcome = client.decide(req).await?;

    let (risk_score, risk_conf) = match outcome.response.answers.get("risk_score") {
        Some(Answer::Score {
            score, confidence, ..
        }) => (*score, *confidence),
        _ => (3.0, 0.0),
    };
    let destructive_prob = match outcome.response.answers.get("is_destructive") {
        Some(Answer::Noul { noul, .. }) => *noul,
        _ => 0.99,
    };
    let secret_prob = match outcome.response.answers.get("exposes_secrets") {
        Some(Answer::Noul { noul, .. }) => *noul,
        _ => 0.99,
    };

    let (action, reason) = decide_hook_gate_action(
        risk_score,
        risk_conf,
        destructive_prob,
        secret_prob,
        outcome.latency_ms,
    );

    Ok(GateEvaluation {
        action,
        fast_path: false,
        reason,
        risk_score,
        risk_confidence: risk_conf,
        destructive_probability: destructive_prob,
        secret_exposure_probability: secret_prob,
        latency_ms: outcome.latency_ms,
    })
}

pub fn decide_hook_gate_action(
    risk_score: f64,
    risk_conf: f64,
    destructive_prob: f64,
    secret_prob: f64,
    latency_ms: u128,
) -> (String, String) {
    // 극단적 파괴·유출 명령(예: rm -rf /, 시크릿 외부 전송 등)은 즉시 하드 차단(deny)
    if (risk_score >= 2.75 && destructive_prob >= 0.90)
        || (risk_score >= 2.65 && secret_prob >= 0.90)
    {
        return (
            "deny".to_string(),
            format!(
                "[Decision Gate 차단] 고위험 파괴·유출 명령 감지 (risk={risk_score:.2}/3, 파괴확률={:.0}%, 비밀노출={:.0}%, {latency_ms}ms)",
                destructive_prob * 100.0,
                secret_prob * 100.0
            ),
        );
    }

    // 비가역 변경·외부 발신·시크릿 접근 의심 시 always-proceed 상태에서도 사용자 확인창 강제(force_ask)
    if risk_score >= 2.00
        || destructive_prob >= 0.55
        || secret_prob >= 0.40
        || (risk_score >= 1.65 && risk_conf < 0.60)
    {
        return (
            "force_ask".to_string(),
            format!(
                "[Decision Gate 확인 필요] risk={risk_score:.2}/3 (확신도 {:.0}%), 파괴확률={:.0}%, 비밀노출={:.0}% ({latency_ms}ms)",
                risk_conf * 100.0,
                destructive_prob * 100.0,
                secret_prob * 100.0
            ),
        );
    }

    // 일반 로컬 개발·빌드·테스트·가역적 편집 명령은 무소음 자동 허용(allow)하여 always-proceed 및 agy-route 흐름을 방해하지 않음
    (
        "allow".to_string(),
        format!("로컬 안전/표준 작업 허용 (risk={risk_score:.2}/3, {latency_ms}ms)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_allows_clean_readonly_and_rejects_chains_or_secrets() {
        assert!(is_zero_ms_safe_command("git status"));
        assert!(is_zero_ms_safe_command("git diff --stat src/"));
        assert!(is_zero_ms_safe_command("cargo check --offline"));
        assert!(is_zero_ms_safe_command("cargo test --lib"));
        assert!(is_zero_ms_safe_command("rg DecisionClient src/"));
        assert!(is_zero_ms_safe_command("go version"));
        assert!(is_zero_ms_safe_command("go test ./..."));

        assert!(!is_zero_ms_safe_command("cat .env"));
        assert!(!is_zero_ms_safe_command("cat ~/.config/agy-decision-lab/*"));
        assert!(!is_zero_ms_safe_command("cat /etc/passwd"));
        assert!(!is_zero_ms_safe_command("cat ../secret.txt"));
        assert!(!is_zero_ms_safe_command("rg AIza src/"));
        assert!(!is_zero_ms_safe_command("cat $HOME/config"));
        assert!(!is_zero_ms_safe_command("git status && rm -rf target"));
        assert!(!is_zero_ms_safe_command("find . -name '*.tmp' -delete"));
        assert!(!is_zero_ms_safe_command("git push --force origin main"));
    }

    #[test]
    fn fast_path_filters_trusted_roots_and_validates_chains_and_cwd() {
        let home = PathBuf::from("/home/alice");
        let roots = filter_safe_trusted_roots(
            [
                "/home/alice",
                "/home/alice/Downloads",
                "/home/alice/.config",
                "/home/alice/.gemini",
                "/etc",
                "/home/alice/dev/repo",
                "/home/alice/.herdr/worktrees/proj/wt1",
                "/opt/workspaces/repo-a",
            ],
            Some(&home),
        );
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/alice/dev/repo"),
                PathBuf::from("/home/alice/.herdr/worktrees/proj/wt1"),
                PathBuf::from("/opt/workspaces/repo-a"),
            ]
        );

        let cwd = Some("/opt/workspaces/repo-a");
        assert!(is_zero_ms_safe_command_with_roots(
            "cat /opt/workspaces/repo-a/src/lib.rs",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(is_zero_ms_safe_command_with_roots(
            "git -C /opt/workspaces/repo-a --no-pager status",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(is_zero_ms_safe_command_with_roots(
            "cargo test --manifest-path /opt/workspaces/repo-a/Cargo.toml",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(is_zero_ms_safe_command_with_roots(
            "git status && git diff --stat src/ && cargo check --offline",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(is_zero_ms_safe_command_with_roots(
            "git -C /home/alice/dev/repo status && go test ./...",
            cwd,
            &roots,
            Some(&home),
        ));

        assert!(!is_zero_ms_safe_command_with_roots(
            "cat /etc/hosts",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "cat /home/alice/.bashrc",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "git -C /etc status",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "git -c diff.external=sh diff",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "git diff --output=/tmp/x",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "go test ../...",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "go env -w GOPROXY=evil",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "git status & pwd",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "git status && rm -rf target",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "git status &&",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "rg 'foo && bar' src/",
            cwd,
            &roots,
            Some(&home),
        ));
        assert!(!is_zero_ms_safe_command_with_roots(
            "cat /home/alice/dev/repo/.config/secret",
            cwd,
            &roots,
            Some(&home),
        ));
    }

    #[test]
    fn gate_thresholds_distinguish_allow_force_ask_and_deny() {
        let (act0, _) = decide_hook_gate_action(0.3, 0.92, 0.03, 0.02, 210);
        assert_eq!(act0, "allow");

        let (act1, _) = decide_hook_gate_action(1.3, 0.85, 0.20, 0.05, 220);
        assert_eq!(act1, "allow");

        let (act2, _) = decide_hook_gate_action(2.1, 0.88, 0.65, 0.05, 250);
        assert_eq!(act2, "force_ask");

        let (act_secret, _) = decide_hook_gate_action(1.02, 0.57, 0.05, 0.41, 240);
        assert_eq!(act_secret, "force_ask");

        let (act3, _) = decide_hook_gate_action(2.92, 0.95, 0.96, 0.10, 260);
        assert_eq!(act3, "deny");
    }

    #[tokio::test]
    async fn protected_credential_store_forces_ask_locally() {
        let eval1 = evaluate_command_for_hook(
            "cat ~/.config/agy-decision-lab/*",
            None,
            Duration::from_secs(2),
        )
        .await
        .expect("자격 증명 저장소 차단 평가가 성공해야 함");
        assert_eq!(eval1.action, "force_ask");
        assert!(eval1.fast_path);

        let eval2 = evaluate_command_for_hook(
            "cat ~/.aws/credentials",
            None,
            Duration::from_secs(2),
        )
        .await
        .expect("AWS 자격 증명 차단 평가가 성공해야 함");
        assert_eq!(eval2.action, "force_ask");
        assert!(eval2.fast_path);
    }
}
