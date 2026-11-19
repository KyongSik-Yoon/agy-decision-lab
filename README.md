# agy-decision-lab (Rust)

[Gemini Decisions API](https://aistudio.google.com/docs/decisions-api)의 전용 판별 모델인 [`gemini-decision-1`](https://aistudio.google.com/docs/models/gemini-decision-1)을 [Antigravity CLI(`agy`)](https://antigravity.google/docs/cli/reference) 워크플로우에 결합한 통합 패키지입니다:

1. **네이티브 `decision` 플러그인 (`plugins/decision`)**: `PreToolUse` 무소음 안전 검문소(`always-proceed` 호환 `allow` 패스스루 + 파괴·유출 고위험 명령만 `force_ask`/`deny`) + `decision_lab` MCP 서버 + `decision-checklist` 스킬 통합
2. **네이티브 `decision-stop` 플러그인 (`plugins/decision-stop`)**: 에이전트 작업 완료(`Stop`) 시 백그라운드 대기·읽기 전용 턴은 0ms 통과시키고 변경된 파일의 Git diff만 증분 스캔해 `critical`/`high` 체크리스트 위반을 검증하는 독립 플러그인
3. **네이티브 `decision-subagent` 플러그인 (`plugins/decision-subagent`)**: 메인 에이전트의 프롬프트 캐시를 깨뜨리지 않고 하위 서브에이전트(`invoke_subagent`) 및 스웜 워커(`swarm add`) 스폰 시점에 `PreToolUse` `overwrite`로 최적 모델(Gemini 4 Hydrogen·Neon·Carbon 11단계)과 추론 강도(`low`/`medium`/`high`/`xhigh`)를 자동 주입하는 독립 플러그인
4. **로컬 stdio MCP 서버 (`agy-decision-mcp`)**: [`rmcp`](https://crates.io/crates/rmcp) 기반 대규모 diff 체크리스트 일괄 스캔(`scan_code_checklist`), 명령 안전 평가(`gate_shell_command`), 멀티모달 다중 판별(`evaluate_decisions`), 다중 저장소 Git 요약(`workspace_git_digest`), 다중 파일 구간·패턴 일괄 수집(`batch_code_context`) 도구 제공

## 빌드·패키징 및 정식 배포 설치

심볼릭 링크 없이 독립된 배포 패키지(`dist/`)를 조립하고 `agy plugin install` 및 `~/.local/bin` 네이티브 바이너리·`~/.config/agy-decision-lab/config.env` 표준 설정으로 설치합니다(설치 후에는 소스 저장소를 이동·삭제해도 단독 동작합니다):

```bash
# 원클릭 빌드 + 패키징(dist/*.tar.gz) + 정식 플러그인·바이너리 설치
./scripts/install.sh

# 설치 없이 배포판 번들 및 tar.gz 아카이브만 생성
./scripts/install.sh --package-only
```

## 주요 제어 명령어

```bash
# 1. 플러그인별 독립 ON/OFF 제어
agy plugin list
agy plugin disable decision-subagent # 서브에이전트·스웜 동적 모델 라우터만 즉시 비활성화
agy plugin disable decision-stop     # Stop 완료 검증만 즉시 비활성화
agy plugin disable decision          # PreToolUse 안전 게이트 + MCP + 스킬 일괄 비활성화

# 2. 훅 세부 토글 및 시뮬레이션 테스트
agy-decision-hook status
agy-decision-hook subagent-off       # 서브에이전트·스웜 동적 라우터만 즉시 일시 중지
agy-decision-hook subagent-on        # 서브에이전트·스웜 동적 라우터 재개
agy-decision-hook subagent-log -n 20 # 실제 라우팅 이력(JSONL) 확인
agy-decision-hook gate-off           # PreToolUse 명령 안전 검문소만 일시 중지
agy-decision-hook stop-off           # Stop 완료 체크리스트 검증만 일시 중지
agy-decision-hook test-subagent "Audit lock ordering across async channels" --kind swarm --workspace inherit
agy-decision-hook test-gate "git push --force origin main"
agy-decision-hook test-stop          # 현재 작업 트리의 git diff로 Stop 검증 시뮬레이션

# 3. 모듈별 선택적 제거 및 서브에이전트 라우터 원복
./scripts/rollback-subagent-router.sh              # 서브에이전트 라우터만 즉시 비활성화·언마운트
./scripts/rollback-subagent-router.sh --hard-reset # 베이스라인 바이너리 및 Git 스냅샷으로 완전 원복
./scripts/uninstall.sh --subagent                  # decision-subagent 플러그인만 제거
./scripts/uninstall.sh --stop                      # decision-stop 플러그인만 제거
./scripts/uninstall.sh --plugin                    # decision 기본 플러그인만 제거
./scripts/uninstall.sh --all                       # 플러그인 3종 및 런타임 캐시 일괄 제거
```

## 보안 및 과금 분리 원칙

- `agy` 본체(`~/.gemini/antigravity-cli/settings.json`)의 Google 계정 로그인 세션(AI Ultra 쿼터)은 변경하지 않고 그대로 유지합니다.
- 플러그인 번들과 `~/.local/bin/{agy-decision-hook,agy-decision-mcp}` 바이너리 내부에서만 `~/.config/agy-decision-lab/config.env`의 `GEMINI_API_KEY`를 독립적으로 읽어 `agy` 메인 세션과 충돌하지 않습니다.
- 사내 민감 코드 diff 스캔 시에는 AI Studio 유료 티어(`$0.10` / 1M 입력 토큰, 출력 `$0`) 프로젝트 키 분리를 권장합니다 ([Gemini Decisions API 가이드](https://flaviocopes.com/gemini-decisions-api/)).
