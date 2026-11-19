use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

#[test]
fn mcp_stdio_lists_five_tools_and_executes_workspace_git_digest() {
    let bin_path = std::env::var("AGY_DECISION_MCP_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_agy-decision-mcp").to_string());

    let mut child = Command::new(&bin_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("agy-decision-mcp 스폰 실패");

    let mut stdin = child.stdin.take().expect("stdin 확보 실패");
    let stdout = child.stdout.take().expect("stdout 확보 실패");
    let mut reader = BufReader::new(stdout);

    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "e2e-test", "version": "0.1.0" }
        }
    });
    writeln!(stdin, "{init_req}").unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).expect("initialize 응답 읽기 실패");
    let init_resp: serde_json::Value = serde_json::from_str(&line).expect("JSON 파싱 실패");
    assert_eq!(init_resp["id"], 1);
    assert!(init_resp["result"]["serverInfo"].is_object());

    let initialized_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{initialized_notif}").unwrap();

    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });
    writeln!(stdin, "{list_req}").unwrap();
    stdin.flush().unwrap();

    line.clear();
    reader.read_line(&mut line).expect("tools/list 응답 읽기 실패");
    let list_resp: serde_json::Value = serde_json::from_str(&line).expect("tools/list JSON 파싱 실패");
    assert_eq!(list_resp["id"], 2);
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools 배열 누락");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "scan_code_checklist",
        "gate_shell_command",
        "evaluate_decisions",
        "workspace_git_digest",
        "batch_code_context",
    ] {
        assert!(
            names.contains(&expected),
            "도구 누락: {expected}, 실제 목록: {names:?}"
        );
    }

    let call_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "workspace_git_digest",
            "arguments": {
                "repo_paths": [env!("CARGO_MANIFEST_DIR")],
                "recent_commits": 2,
                "include_diff_patch": false
            }
        }
    });
    writeln!(stdin, "{call_req}").unwrap();
    stdin.flush().unwrap();

    line.clear();
    reader.read_line(&mut line).expect("tools/call 응답 읽기 실패");
    let call_resp: serde_json::Value = serde_json::from_str(&line).expect("tools/call JSON 파싱 실패");
    assert_eq!(call_resp["id"], 3);
    assert_ne!(call_resp["result"]["isError"], true);
    let payload_text = call_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("tools/call 텍스트 페이로드 누락");
    let payload: serde_json::Value =
        serde_json::from_str(payload_text).expect("도구 응답 JSON 파싱 실패");
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["repo_count"], 1);
    assert!(payload["repos"][0]["branch"].as_str().is_some());

    drop(stdin);
    let _ = child.wait();
}
