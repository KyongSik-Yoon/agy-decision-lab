---
name: subagent-routing
description: Gemini Decision 1을 이용해 서브에이전트(invoke_subagent) 및 네이티브 스웜(swarm add) 스폰 시점에 최적 모델과 추론 강도를 자동 세팅하는 decision-subagent 플러그인 제어·튜닝·원복 가이드. 트리거 — '서브에이전트 라우터', '스웜 모델 자동 배정', 'subagent-off', '라우터 원복'.
---

# 서브에이전트·스웜 동적 모델 라우터 가이드 (`decision-subagent`)

## 1. 동작 원리 (프롬프트 캐시 100% 보존 + 추가 턴 0회)

- `PreToolUse` 훅이 `invoke_subagent` 및 `swarm`(`action: "add"`) 호출 직전에 `Prompt` / `brief`를 읽어 `gemini-decision-1`(단일 패스 약 200~250ms)로 작업 티어와 추론 깊이를 판별한 뒤 `overwrite`로 모델 인자를 주입한다.
- **`swarm` (`action: "add"`)**: `gemini-3.8-flash-{low,medium,high}`, `gemini-4-argon-{low,medium,high}`, `gemini-4-carbon-{low,medium,high}` 9단계 슬러그로 **모델과 추론 강도(`effort`)를 동시에 세팅**한다.
- **`invoke_subagent`**: 스키마가 허용하는 `flash_lite` / `flash` / `pro` / `inherit` 4단계 티어로 매핑한다.

## 2. 성능 저하 방지 안전장치

- **작성자 하한선(`author_floor_argon`)**: 코드를 수정하는 워커(`workspace: "share" | "branch"` 또는 `fork` 지정)는 절대 `flash`로 강등되지 않고 최소 `gemini-4-argon-low/medium` 이상을 보장한다.
- **타 패밀리 교차 검증 보존(`preserve_cross_family`)**: `claude-*` / `gpt-*` 모델을 명시한 스웜 워커는 패밀리를 유지하며, `claude-*`에서 복잡도가 높을 때만 추론 강도(`-medium`/`-high`)를 상향한다.
- **Fail-Open**: API 지연·오류 시 차단 없이 원래 호출 인자 그대로 즉시 통과한다.

## 3. 즉시 제어·시뮬레이션·원복 명령

```bash
# 현재 상태 및 최근 라우팅 로그 확인
agy-decision-hook status
agy-decision-hook subagent-log -n 20

# 재시작 없이 즉시 일시 중지 / 재개
agy-decision-hook subagent-off
agy-decision-hook subagent-on

# 시뮬레이션 테스트
agy-decision-hook test-subagent "Audit lock ordering and race conditions in dispatcher" --kind swarm --workspace inherit
agy-decision-hook test-subagent "Find all YAML config files" --kind subagent --type-name research

# 플러그인 비활성화 또는 완전 원복
agy plugin disable decision-subagent
./scripts/rollback-subagent-router.sh
```
