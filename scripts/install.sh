#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 소스 저장소에서 실행할 때는 상위 폴더가 루트, 배포 타르볼 루트에서 실행할 때는 현재 폴더가 루트
if [ -f "$SCRIPT_DIR/../Cargo.toml" ]; then
  ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
  DIST_DIR="$ROOT_DIR/dist"
else
  ROOT_DIR="$SCRIPT_DIR"
  DIST_DIR="$SCRIPT_DIR"
fi

PACKAGE_ONLY=false
FROM_DIST=false

usage() {
  cat <<'EOF'
사용법: ./scripts/install.sh [옵션]

옵션:
  (기본값)         릴리스 바이너리 컴파일 -> 자립형 배포 번들(dist/) 패키징 -> XDG 설정 구성 ->
                   구형 심볼릭 링크 정리 -> agy plugin install 정식 설치 및 ~/.local/bin 바이너리 배포
  --package-only   설치는 수행하지 않고 dist/ 번들과 포터블 tar.gz 배포 아카이브만 생성
  --from-dist      소스 컴파일 없이 이미 빌드된 dist/ 산출물만으로 정식 설치 수행
  -h, --help       도움말 표시
EOF
}

for arg in "$@"; do
  case "$arg" in
    --package-only) PACKAGE_ONLY=true ;;
    --from-dist)    FROM_DIST=true ;;
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

BINS=(agy-decision-hook agy-decision-mcp agy-route agy-acp)
PLUGINS=(decision decision-stop decision-subagent)

if ! $FROM_DIST; then
  echo "[1/5] 릴리스 바이너리 빌드 중 (strip 활성화)..."
  cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --release \
    --bin agy-decision-hook \
    --bin agy-decision-mcp \
    --bin agy-route \
    --bin agy-acp

  echo "[2/5] 자립형 배포 패키지 조립 중 ($DIST_DIR)..."
  rm -rf "$DIST_DIR"
  mkdir -p "$DIST_DIR/bin" "$DIST_DIR/plugins" "$DIST_DIR/rules" "$DIST_DIR/scripts"

  for b in "${BINS[@]}"; do
    install -m 0755 "$ROOT_DIR/target/release/$b" "$DIST_DIR/bin/$b"
  done
  install -m 0644 "$ROOT_DIR/rules/default_rules.yaml" "$DIST_DIR/rules/default_rules.yaml"
  install -m 0644 "$ROOT_DIR/.env.example" "$DIST_DIR/config.env.example"

  # 1) decision 플러그인 번들 (안전 게이트 훅 + MCP 서버 + 체크리스트 스킬 + 데이터 내장)
  cp -R "$ROOT_DIR/plugins/decision" "$DIST_DIR/plugins/decision"
  mkdir -p "$DIST_DIR/plugins/decision/bin" "$DIST_DIR/plugins/decision/data"
  install -m 0755 "$DIST_DIR/bin/agy-decision-hook" "$DIST_DIR/plugins/decision/bin/agy-decision-hook"
  install -m 0755 "$DIST_DIR/bin/agy-decision-mcp" "$DIST_DIR/plugins/decision/bin/agy-decision-mcp"
  install -m 0644 "$DIST_DIR/rules/default_rules.yaml" "$DIST_DIR/plugins/decision/data/default_rules.yaml"

  # 2) decision-stop 플러그인 번들 (Stop diff 감사 훅 + 데이터 내장)
  cp -R "$ROOT_DIR/plugins/decision-stop" "$DIST_DIR/plugins/decision-stop"
  mkdir -p "$DIST_DIR/plugins/decision-stop/bin" "$DIST_DIR/plugins/decision-stop/data"
  install -m 0755 "$DIST_DIR/bin/agy-decision-hook" "$DIST_DIR/plugins/decision-stop/bin/agy-decision-hook"
  install -m 0644 "$DIST_DIR/rules/default_rules.yaml" "$DIST_DIR/plugins/decision-stop/data/default_rules.yaml"

  # 3) decision-subagent 플러그인 번들 (서브에이전트·스웜 동적 모델·추론 강도 라우터 훅)
  cp -R "$ROOT_DIR/plugins/decision-subagent" "$DIST_DIR/plugins/decision-subagent"
  mkdir -p "$DIST_DIR/plugins/decision-subagent/bin"
  install -m 0755 "$DIST_DIR/bin/agy-decision-hook" "$DIST_DIR/plugins/decision-subagent/bin/agy-decision-hook"

  install -m 0755 "$ROOT_DIR/scripts/install.sh" "$DIST_DIR/scripts/install.sh"
  install -m 0755 "$ROOT_DIR/scripts/uninstall.sh" "$DIST_DIR/scripts/uninstall.sh"

  echo "[3/5] 플러그인 번들 정합성 검증 (agy plugin validate --strict)..."
  for p in "${PLUGINS[@]}"; do
    agy plugin validate "$DIST_DIR/plugins/$p" --strict >/dev/null
    echo "  ✔ $p 검증 통과"
  done

  TARBALL="$DIST_DIR/agy-decision-lab-linux-x86_64.tar.gz"
  tar -C "$DIST_DIR" -czf "$TARBALL" bin plugins rules scripts config.env.example
  echo "  ✔ 포터블 아카이브 생성: $TARBALL"
fi

if $PACKAGE_ONLY; then
  echo "패키징 완료 (--package-only)."
  exit 0
fi

echo "[4/5] 표준 XDG 설정 디렉터리 및 네이티브 CLI 바이너리 배포 중..."
XDG_CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/agy-decision-lab"
mkdir -p "$XDG_CFG_DIR/rules" "$HOME/.local/bin"

if [ ! -f "$XDG_CFG_DIR/rules/default_rules.yaml" ]; then
  install -m 0644 "$DIST_DIR/rules/default_rules.yaml" "$XDG_CFG_DIR/rules/default_rules.yaml"
  echo "  ✔ 기본 규칙 배포: $XDG_CFG_DIR/rules/default_rules.yaml"
fi

if [ ! -f "$XDG_CFG_DIR/config.env" ]; then
  umask 077
  if [ -f "$ROOT_DIR/.env" ]; then
    # 개발 저장소 경로 하드코딩(AGY_RULES_PATH=.../git-repo/...)은 제거하고 순수 환경설정만 마이그레이션
    grep -vE '^AGY_RULES_PATH=.*git-repo' "$ROOT_DIR/.env" > "$XDG_CFG_DIR/config.env"
    chmod 0600 "$XDG_CFG_DIR/config.env"
    echo "  ✔ 기존 .env 설정을 표준 경로로 안전 이전 (0600): $XDG_CFG_DIR/config.env"
  else
    install -m 0600 "$DIST_DIR/config.env.example" "$XDG_CFG_DIR/config.env"
    echo "  ! 템플릿 생성됨 ($XDG_CFG_DIR/config.env) — GEMINI_API_KEY를 입력해 주세요."
  fi
else
  chmod 0600 "$XDG_CFG_DIR/config.env"
  echo "  ✔ 기존 XDG 설정 유지: $XDG_CFG_DIR/config.env"
fi

# 구형 .sh 래퍼 스크립트까지 모두 네이티브 ELF 바이너리로 원자적 교체
for b in "${BINS[@]}"; do
  rm -f "$HOME/.local/bin/$b"
  install -m 0755 "$DIST_DIR/bin/$b" "$HOME/.local/bin/$b"
done
echo "  ✔ 네이티브 바이너리 4종 배포 완료 ($HOME/.local/bin/{agy-decision-hook,agy-decision-mcp,agy-route,agy-acp})"

echo "[5/5] 레거시 심볼릭 링크 정리 및 agy plugin install 정식 패키지 설치 중..."
for p in "${PLUGINS[@]}"; do
  LEGACY_LINK="$HOME/.gemini/config/plugins/$p"
  if [ -L "$LEGACY_LINK" ]; then
    rm -f "$LEGACY_LINK"
    echo "  - 구형 심볼릭 링크 제거: $LEGACY_LINK"
  fi
  agy plugin install "$DIST_DIR/plugins/$p"
  agy plugin enable "$p" >/dev/null 2>&1 || true
done

echo ""
echo "설치 완료! 이제 소스 저장소 경로와 완전히 분리된 정식 빌드 배포물로 동작합니다."
"$HOME/.local/bin/agy-decision-hook" status
