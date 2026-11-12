# agy-decision-lab (Rust)

[Gemini Decisions API](https://aistudio.google.com/docs/decisions-api)의 전용 판별 모델인 [`gemini-decision-1`](https://aistudio.google.com/docs/models/gemini-decision-1)을 [Antigravity CLI(`agy`)](https://antigravity.google/docs/cli/reference) 워크플로우에 결합한 통합 패키지입니다:

1. **멀티턴 동적 라우터 (`agy-route`)**: 매 질문마다 최적 모델·`--effort`를 새로 판별해 동일 대화 세션에 연속 호출하는 REPL 및 진입 래퍼 (`agy-route "질문"` 실행 시 `exec agy --model <모델> -i "질문"`으로 순정 TUI에 즉시 핸드오프)
2. **상주 ACP 동적 모델 스위치 클라이언트·프록시 (`agy-acp`)**: 단일 `agy acp` 프로세스를 상주시켜 콜드 스타트(0ms) 없이 매 턴 `session/set_model`로 실시간 모델을 교체하는 REPL·단발 실행기 및 표준 ACP 에디터 프록시(`--stdio-proxy`)
3. **네이티브 `decision` 플러그인 (`plugins/decision`)**: `PreToolUse` 무소음 안전 검문소(`always-proceed` 호환 `allow` 패스스루 + 파괴·유출 고위험 명령만 `force_ask`/`deny`) + `decision_lab` MCP 서버 + `decision-checklist` 스킬 통합
4. **네이티브 `decision-stop` 플러그인 (`plugins/decision-stop`)**: 에이전트 작업 완료(`Stop`) 시 Git diff가 있을 때만 `critical`/`high` 체크리스트 위반(시크릿 노출 등)을 단일 패스 스캔하여 자동 보완을 유도하는 독립 플러그인
5. **로컬 stdio MCP 서버 (`agy-decision-mcp`)**: [`rmcp`](https://crates.io/crates/rmcp) 기반 대규모 diff 체크리스트 일괄 스캔 및 멀티모달 다중 판별 도구

## 주요 제어 명령어

```bash
# 1. 멀티턴 모델 자동 교체 REPL 시작 (기존 agy-route 유지)
agy-route

# 2. 상주 ACP 세션 위에서 콜드 스타트 없이 실시간 모델 교체 (REPL / 원샷 / 에디터 프록시)
agy-acp                          # 상주 ACP 멀티턴 REPL (/model, /resume, /paste, /tui 지원)
agy-acp -p "질문 내용"           # 원샷 실행
agy-acp --dry-run "질문 내용"    # API 라우팅 결과만 확인
agy-acp --stdio-proxy            # Zed / Neovim 등 외부 ACP 클라이언트용 자동 모델 라우팅 프록시

# 3. 플러그인별 독립 ON/OFF 제어
agy plugin list
agy plugin disable decision-stop # Stop 완료 검증만 즉시 비활성화
agy plugin disable decision      # PreToolUse 안전 게이트 + MCP + 스킬 일괄 비활성화

# 4. 훅 세부 토글 및 시뮬레이션 테스트
agy-decision-hook status
agy-decision-hook gate-off       # PreToolUse 명령 안전 검문소만 일시 중지
agy-decision-hook stop-off       # Stop 완료 체크리스트 검증만 일시 중지
agy-decision-hook test-gate "git push --force origin main"
agy-decision-hook test-stop      # 현재 작업 트리의 git diff로 Stop 검증 시뮬레이션

# 5. 모듈별 선택적 제거 (기존 agy-route는 건드리지 않음)
./scripts/uninstall.sh --acp     # agy-acp 래퍼만 제거
./scripts/uninstall.sh --stop    # decision-stop 플러그인만 제거
./scripts/uninstall.sh --plugin  # decision 기본 플러그인만 제거
./scripts/uninstall.sh --all     # 위 3개 모두 제거 (agy-route 유지)
```

## 보안 및 과금 분리 원칙

- `agy` 본체(`~/.gemini/antigravity-cli/settings.json`)의 Google 계정 로그인 세션(AI Ultra 쿼터)은 변경하지 않고 그대로 유지합니다.
- `~/.local/bin/{agy-route,agy-decision-hook,agy-decision-mcp}` 래퍼 내부에서만 `.env`의 `GEMINI_API_KEY`를 읽으며, `agy-route`는 자식 `agy` 실행 직전 해당 환경변수를 제거해 세션 충돌을 막습니다.
- 사내 민감 코드 diff 스캔 시에는 AI Studio 유료 티어(`$0.10` / 1M 입력 토큰, 출력 `$0`) 프로젝트 키 분리를 권장합니다 ([Gemini Decisions API 가이드](https://flaviocopes.com/gemini-decisions-api/)).
