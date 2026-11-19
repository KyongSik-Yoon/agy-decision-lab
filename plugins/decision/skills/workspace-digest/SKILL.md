---
name: workspace-digest
description: 다중 Git 저장소 상태·커밋·diff 통계 요약(workspace_git_digest)이나 여러 파일 라인 구간·대용량 로그 tail·패턴 컨텍스트 일괄 수집(batch_code_context)이 필요할 때 `python3 -c`나 복합 파이프 셸 명령 대신 사용한다. 트리거 — 다중 저장소 Git 요약, 여러 파일 동시 슬라이스/로그 꼬리/패턴 컨텍스트 추출. 단건 파일 열람(read_file)·단순 단일 명령(run_command)에는 사용하지 않는다.
---

# 워크스페이스 일괄 컨텍스트 수집 가이드 (`decision_lab` MCP)

`decision` 플러그인의 `decision_lab` MCP 서버에는 다단계 `python3 -c` 또는 복합 파이프 셸을 대체해 왕복 턴과 안전 모니터 지연을 줄이는 순수 읽기·집계 도구 2종이 포함되어 있다. 호출 전 `<appDataDir>/mcp/decision_lab/<toolName>.json` 스키마를 확인한 뒤 `call_mcp_tool`(`ServerName: "decision_lab"`)로 실행한다.

## 1. 도구 선택 기준

- **`workspace_git_digest`**: 1개 또는 여러 Git 저장소(`repo_paths`, 최대 16개)의 브랜치, upstream/ahead/behind, staged/unstaged/untracked 파일 요약, 최근 커밋, diff 통계를 단일 호출로 수집한다.
- **`batch_code_context`**: 여러 파일의 라인 구간·로그 tail(`slices`, 구간당 최대 400줄)과 ripgrep 패턴 컨텍스트(`searches`)를 총 글자 수 예산(`max_total_chars`) 안에서 일괄 추출한다(민감 시크릿 파일은 자동 차단).

## 2. 사용 자제 기준 (패스트패스 우선)

- 단건 파일 읽기(`read_file`)나 단일 `git`·`rg` 명령(`run_command` 병렬 호출)으로 끝나는 단순 작업은 Lazy MCP 스키마 조회 1턴 + `call_mcp_tool` 고정 심사 비용 때문에 오히려 느리므로 내장 도구를 우선한다.
