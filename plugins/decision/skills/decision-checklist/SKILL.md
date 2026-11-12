---
name: decision-checklist
description: Gemini Decision 1(decision_lab MCP 서버)을 사용해 git diff·코드·문서를 보안·신뢰성·호환성 다중 체크리스트로 단일 패스 심사하거나 커스텀 noul/choice/score 판정을 수행할 때 사용한다. 트리거 — '디시즌 스캔', '체크리스트 검사', 'diff 보안 스캔', 'decision 모델로 심사'.
---

# Decision 체크리스트 심사 가이드

`decision` 플러그인이 마운트하는 `decision_lab` MCP 서버의 도구들은 Lazy 로드이므로, 호출 전 `<appDataDir>/mcp/decision_lab/<toolName>.json` 스키마를 확인한 뒤 `call_mcp_tool`(`ServerName: "decision_lab"`)로 실행한다.

## 1. 주요 도구 선택 기준

- **`scan_code_checklist`**: `git diff` 결과나 수정된 소스코드를 기본 규칙(`hardcoded_credentials`, `injection_vulnerability`, `destructive_fs_or_db_op`, `concurrency_or_lock_hazard` 등) 및 즉석 `extra_checks`(`noul` 긍정문 질문)로 한 번의 패스(약 250ms)에 일괄 검증한다.
- **`evaluate_decisions`**: 텍스트 또는 로컬 이미지·PDF(`local_file_path`)를 대상으로 커스텀 `noul`(확률 0.01~0.99), `choice`(다지선다), `score`(루브릭 점수) 질문을 최대 1,000개까지 동시 평가한다.
- **`gate_shell_command`**: 배포 스크립트나 고위험 운영 명령을 실행하기 전 폭발 반경(`risk_score` 0~3)과 확신도를 정량 평가할 때 사용한다.

## 2. 질문 작성 규칙

- `noul` 질문의 `instructions`는 부정어를 피하고 명확한 영어 긍정 의문문(`"Does the diff modify database schema?"`)으로 작성한다.
