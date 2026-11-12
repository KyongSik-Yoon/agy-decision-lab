#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
사용법: ./scripts/uninstall.sh [옵션]

옵션 (여러 개 조합 가능):
  --acp       2번(agy-acp 상주 모델 스위칭 라우터/프록시 래퍼: ~/.local/bin/agy-acp)만 제거
  --subagent  4번(decision-subagent 서브에이전트·스웜 동적 모델 라우터 플러그인)만 제거
  --stop      3번(decision-stop 완료 체크리스트 검증 플러그인)만 제거
  --plugin    1번(decision 기본 안전 게이트 + MCP 플러그인)만 제거
  --all       이번에 구성한 decision 플러그인 3종, agy-acp 래퍼, 런타임 임시 캐시를 모두 제거
             (기존 ~/.local/bin/agy-route 및 ~/.gemini/config/{hooks,mcp_config}.json 의 herdr·asana 는 보존)
  -h, --help 도움말 표시
EOF
}

if [ $# -eq 0 ]; then
  usage
  exit 1
fi

DO_ACP=false
DO_SUBAGENT=false
DO_STOP=false
DO_PLUGIN=false
DO_CACHE=false

for arg in "$@"; do
  case "$arg" in
    --acp)      DO_ACP=true ;;
    --subagent) DO_SUBAGENT=true ;;
    --stop)     DO_STOP=true ;;
    --plugin)   DO_PLUGIN=true ;;
    --all)
      DO_ACP=true
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

if $DO_ACP; then
  remove_entry "$HOME/.local/bin/agy-acp" "agy-acp 상주 라우터/프록시 래퍼"
fi

if $DO_SUBAGENT; then
  remove_entry "$HOME/.gemini/config/plugins/decision-subagent" "decision-subagent (서브에이전트·스웜 라우터) 플러그인"
fi

if $DO_STOP; then
  remove_entry "$HOME/.gemini/config/plugins/decision-stop" "decision-stop (Stop 체크리스트) 플러그인"
fi

if $DO_PLUGIN; then
  remove_entry "$HOME/.gemini/config/plugins/decision" "decision (PreToolUse + MCP) 플러그인"
  rm -rf "$HOME/.gemini/antigravity-cli/mcp/decision_lab" 2>/dev/null || true
fi

if $DO_CACHE; then
  rm -f "${XDG_RUNTIME_DIR:-/tmp}/agy-decision-stop-dedup.json" \
        "${XDG_RUNTIME_DIR:-/tmp}/agy-decision-hook-cache.json" \
        "${XDG_RUNTIME_DIR:-/tmp}/agy-decision-last-route.json" 2>/dev/null || true
  echo "[정리됨] 런타임 디듑/상태 캐시"
fi

echo "완료. 현재 활성 플러그인 상태 확인: agy plugin list"
