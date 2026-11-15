#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"

usage() {
  cat <<'EOF'
사용법: ./scripts/uninstall.sh [옵션]

옵션 (여러 개 조합 가능):
  --subagent     decision-subagent(서브에이전트·스웜 동적 모델 라우터) 플러그인만 제거
  --stop         decision-stop(Stop 완료 체크리스트 검증) 플러그인만 제거
  --plugin       decision(PreToolUse 안전 게이트 + MCP) 기본 플러그인만 제거
  --cli          ~/.local/bin/{agy-decision-hook,agy-decision-mcp} 바이너리 제거 (레거시 agy-route/agy-acp 포함 정리)
  --purge-config ~/.config/agy-decision-lab 설정 및 ~/.local/state/agy-decision-lab 상태까지 완전 삭제
  --all          플러그인 3종 및 런타임 임시 캐시를 모두 제거
                 (기본적으로 ~/.config/agy-decision-lab/config.env 의 API 키는 보존하며, 삭제하려면 --purge-config 추가)
  -h, --help     도움말 표시
EOF
}

if [ $# -eq 0 ]; then
  usage
  exit 1
fi

DO_SUBAGENT=false
DO_STOP=false
DO_PLUGIN=false
DO_CLI=false
DO_PURGE_CFG=false
DO_CACHE=false

for arg in "$@"; do
  case "$arg" in
    --subagent)     DO_SUBAGENT=true ;;
    --stop)         DO_STOP=true ;;
    --plugin)       DO_PLUGIN=true ;;
    --cli)          DO_CLI=true ;;
    --purge-config) DO_PURGE_CFG=true ;;
    --all)
      DO_SUBAGENT=true
      DO_STOP=true
      DO_PLUGIN=true
      DO_CACHE=true
      ;;
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

uninstall_plugin() {
  local name=$1
  local label=$2
  if command -v agy >/dev/null 2>&1; then
    if agy plugin uninstall "$name" >/dev/null 2>&1; then
      echo "[제거됨] $label (agy plugin uninstall $name)"
    fi
  fi
  local legacy_link="$HOME/.gemini/config/plugins/$name"
  if [ -L "$legacy_link" ] || [ -e "$legacy_link" ]; then
    rm -rf "$legacy_link"
    echo "[제거됨] $label 레거시 경로 ($legacy_link)"
  fi
}

remove_entry() {
  local target=$1
  local label=$2
  if [ -L "$target" ] || [ -e "$target" ]; then
    rm -rf "$target"
    echo "[제거됨] $label ($target)"
  else
    echo "[스킵]   $label 이미 없음 ($target)"
  fi
}

if $DO_SUBAGENT; then
  uninstall_plugin "decision-subagent" "decision-subagent (서브에이전트·스웜 라우터) 플러그인"
fi

if $DO_STOP; then
  uninstall_plugin "decision-stop" "decision-stop (Stop 체크리스트) 플러그인"
fi

if $DO_PLUGIN; then
  uninstall_plugin "decision" "decision (PreToolUse + MCP) 플러그인"
  rm -rf "$HOME/.gemini/antigravity-cli/mcp/decision_lab" 2>/dev/null || true
fi

if $DO_CLI; then
  for b in agy-decision-hook agy-decision-mcp; do
    remove_entry "$HOME/.local/bin/$b" "$b CLI 바이너리"
  done
  for legacy in agy-route agy-acp; do
    if [ -L "$HOME/.local/bin/$legacy" ] || [ -e "$HOME/.local/bin/$legacy" ]; then
      remove_entry "$HOME/.local/bin/$legacy" "$legacy 레거시 바이너리"
    fi
  done
fi

if $DO_CACHE; then
  rm -f "${XDG_RUNTIME_DIR:-/tmp}/agy-decision-stop-dedup.json" \
        "${XDG_RUNTIME_DIR:-/tmp}/agy-decision-hook-cache.json" \
        "${XDG_RUNTIME_DIR:-/tmp}/agy-decision-last-route.json" 2>/dev/null || true
  echo "[정리됨] 런타임 디듑/상태 캐시"
fi

if $DO_PURGE_CFG; then
  remove_entry "${XDG_CONFIG_HOME:-$HOME/.config}/agy-decision-lab" "XDG 설정 디렉터리"
  remove_entry "$HOME/.local/state/agy-decision-lab" "로컬 상태/로그 디렉터리"
fi

echo "완료. 현재 활성 플러그인 상태 확인: agy plugin list"
