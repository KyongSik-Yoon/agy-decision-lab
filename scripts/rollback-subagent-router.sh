#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PLUGIN_LINK="$HOME/.gemini/config/plugins/decision-subagent"
STATE_DIR="$HOME/.local/state/agy-decision-lab"
BACKUP_BIN="$STATE_DIR/backups/agy-decision-hook.baseline"

usage() {
  cat <<'EOF'
사용법: ./scripts/rollback-subagent-router.sh [옵션]

옵션:
  (기본값)       소프트 원복: 정식 설치된 decision-subagent 플러그인 제거(agy plugin uninstall) 및
                 런타임 subagent-disabled 킬스위치 활성화 (기존 decision, decision-stop은 그대로 유지)
  --hard-reset   완전 원복: 소프트 원복 + 백업된 기존 바이너리 복원 + Git baseline-before-subagent-router 체크아웃
  -h, --help     도움말 표시
EOF
}

HARD_RESET=false
for arg in "$@"; do
  case "$arg" in
    --hard-reset) HARD_RESET=true ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "알 수 없는 옵션: $arg" >&2
      usage
      exit 1
      ;;
  esac
done

mkdir -p "$STATE_DIR"
printf 'disabled\n' > "$STATE_DIR/subagent-disabled"
echo "[완료] 런타임 킬스위치 활성화 ($STATE_DIR/subagent-disabled)"

if command -v agy >/dev/null 2>&1; then
  if agy plugin uninstall decision-subagent >/dev/null 2>&1; then
    echo "[완료] 정식 설치 플러그인 제거 (agy plugin uninstall decision-subagent)"
  fi
fi

if [ -L "$PLUGIN_LINK" ] || [ -e "$PLUGIN_LINK" ]; then
  rm -rf "$PLUGIN_LINK"
  echo "[완료] 레거시 플러그인 경로 제거 ($PLUGIN_LINK)"
fi

if $HARD_RESET && [ -d "$REPO_DIR/.git" ]; then
  if [ -f "$BACKUP_BIN" ]; then
    install -m 0755 "$BACKUP_BIN" "$HOME/.local/bin/agy-decision-hook"
    echo "[완료] 베이스라인 바이너리 복원 ($HOME/.local/bin/agy-decision-hook)"
  fi

  if git -C "$REPO_DIR" rev-parse --verify baseline-before-subagent-router >/dev/null 2>&1; then
    git -C "$REPO_DIR" checkout baseline-before-subagent-router -- src plugins scripts README.md .env.example Cargo.toml Cargo.lock
    rm -rf "$REPO_DIR/plugins/decision-subagent" "$REPO_DIR/src/subagent_router.rs" "$REPO_DIR/src/runtime_config.rs"
    echo "[완료] 소스 트리를 baseline-before-subagent-router 스냅샷으로 복원했습니다."
  fi
fi

echo "원복 완료. 순정 서브에이전트·스웜 동작으로 즉시 복귀했습니다."
