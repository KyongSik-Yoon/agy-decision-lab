#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_LINK="$HOME/.gemini/config/plugins/decision-subagent"
STATE_DIR="$HOME/.local/state/agy-decision-lab"
BACKUP_BIN="$STATE_DIR/backups/agy-decision-hook.baseline"
TARGET_BIN="$REPO_DIR/target/release/agy-decision-hook"

usage() {
  cat <<'EOF'
사용법: ./scripts/rollback-subagent-router.sh [옵션]

옵션:
  (기본값)       소프트 원복: decision-subagent 플러그인 심볼릭 링크 제거 및 subagent-disabled 플래그 활성화
                 (기존 decision 안전 게이트, decision-stop, MCP 서버는 그대로 유지)
  --hard-reset   완전 원복: 소프트 원복 + 백업된 기존 agy-decision-hook 바이너리 복원 + Git baseline-before-subagent-router 체크아웃
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

if [ -L "$PLUGIN_LINK" ] || [ -e "$PLUGIN_LINK" ]; then
  rm -rf "$PLUGIN_LINK"
  echo "[완료] 플러그인 마운트 제거 ($PLUGIN_LINK)"
else
  echo "[스킵] 플러그인 마운트 이미 없음 ($PLUGIN_LINK)"
fi

if $HARD_RESET; then
  if [ -f "$BACKUP_BIN" ]; then
    cp -pf "$BACKUP_BIN" "$TARGET_BIN"
    echo "[완료] 베이스라인 릴리스 바이너리 복원 ($TARGET_BIN)"
  else
    echo "[경고] 백업 바이너리를 찾을 수 없어 재빌드를 시도합니다 ($BACKUP_BIN)" >&2
  fi

  if git -C "$REPO_DIR" rev-parse --verify baseline-before-subagent-router >/dev/null 2>&1; then
    git -C "$REPO_DIR" checkout baseline-before-subagent-router -- src plugins scripts README.md .env.example Cargo.toml Cargo.lock
    rm -rf "$REPO_DIR/plugins/decision-subagent" "$REPO_DIR/src/subagent_router.rs"
    echo "[완료] 소스 트리를 baseline-before-subagent-router 스냅샷으로 완전 복원했습니다."
  fi
fi

echo "원복 완료. 기존 순정 서브에이전트·스웜 동작으로 즉시 복귀했습니다."
