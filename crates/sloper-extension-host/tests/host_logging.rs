mod support {
    pub(crate) mod host_fixture;
}

use std::sync::{
    Arc,
    atomic::Ordering,
};

use sloper_extension_host::{
    Engine,
    LogKind,
};
use support::host_fixture::{
    TestHost,
    request,
    run,
};

const NOTICE: &str = "Further attempt logs were dropped after the 1 MiB limit.";
const ENTRY_BYTES: usize = 192;
const LIMIT: usize = 1024 * 1024;

#[tokio::test]
async fn issued_token_is_redacted_from_guest_logs() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    run(&engine, request("allowed"), Arc::clone(&host), None)
        .await
        .expect("declared connection succeeds");
    assert_eq!(host.token_calls.load(Ordering::Acquire), 1);
    let logs = host.logs.lock().expect("log mutex is not poisoned").clone();
    assert_eq!(logs.len(), 1);
    assert!(
        !logs[0].contains("sensitive-test-token"),
        "issued token leaked: {logs:?}"
    );
}

#[tokio::test]
async fn empty_guest_logs_pay_metadata_and_emit_one_reserved_notice() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    let mut input = request("log-flood");
    input.parameters = serde_json::json!({"count":20_000,"message":""}).to_string();
    run(&engine, input, Arc::clone(&host), None)
        .await
        .expect("empty log flood finishes");
    let logs = host.logs.lock().expect("log mutex is not poisoned");
    let accepted = (LIMIT - 4096 - ENTRY_BYTES) / ENTRY_BYTES;
    assert_eq!(logs.len(), accepted + 1);
    assert!(logs[..accepted].iter().all(String::is_empty));
    assert_eq!(logs.last().map(String::as_str), Some(NOTICE));
    assert!(logs.iter().map(|line| line.len() + ENTRY_BYTES).sum::<usize>() <= LIMIT);
}

#[tokio::test]
async fn guest_log_redaction_and_unicode_truncation_are_charged_before_admission() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost {
        token: Some("x".into()),
        ..TestHost::default()
    });
    let mut input = request("log-flood");
    input.parameters = serde_json::json!({"count":400,"message":"x🦀".repeat(1024),"redact":true}).to_string();
    run(&engine, input, Arc::clone(&host), None)
        .await
        .expect("redacted log flood finishes");
    let logs = host.logs.lock().expect("log mutex is not poisoned");
    assert!(logs.iter().all(|line| line.len() <= 4096));
    assert!(logs.iter().all(|line| !line.contains('x')));
    assert!(logs.iter().map(|line| line.len() + ENTRY_BYTES).sum::<usize>() <= LIMIT);
    assert_eq!(logs.iter().filter(|line| line.as_str() == NOTICE).count(), 1);
    assert_eq!(logs.last().map(String::as_str), Some(NOTICE));
    assert!(!logs.iter().any(|line| line == "must-not-appear-after-drop"));
}

#[tokio::test]
async fn guest_notice_text_cannot_forge_internal_budget_control() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    let mut input = request("log-flood");
    input.parameters = serde_json::json!({"count":100,"message":NOTICE}).to_string();
    run(&engine, input, Arc::clone(&host), None)
        .await
        .expect("notice flood finishes");
    let logs = host.logs.lock().expect("log mutex is not poisoned");
    assert_eq!(logs.len(), 101);
    assert!(logs[..100].iter().all(|message| message == NOTICE));
    assert_eq!(logs[100], "must-not-appear-after-drop");
    assert!(
        host.log_kinds
            .lock()
            .expect("log kind mutex")
            .iter()
            .all(|kind| *kind == LogKind::Message)
    );
}

#[tokio::test]
async fn actual_guest_flood_redacts_short_exact_notice_and_marker_tokens() {
    let engine = Engine::new().expect("engine config is valid");
    for token in ["s", NOTICE, "a", "[redacted]"] {
        let host = Arc::new(TestHost {
            token: Some(token.into()),
            ..TestHost::default()
        });
        let mut input = request("log-flood");
        input.parameters = serde_json::json!({"count":20_000,"message":"","redact":true}).to_string();
        run(&engine, input, Arc::clone(&host), None)
            .await
            .expect("redacted guest flood finishes");
        let logs = host.logs.lock().expect("log mutex");
        let kinds = host.log_kinds.lock().expect("log kind mutex");
        assert!(logs.iter().all(|message| !message.contains(token)));
        assert!(logs.iter().all(|message| message.len() <= 4096));
        assert!(logs.iter().map(|message| message.len() + ENTRY_BYTES).sum::<usize>() <= LIMIT);
        assert_eq!(kinds.iter().filter(|kind| **kind == LogKind::DroppedNotice).count(), 1);
        assert_eq!(kinds.last(), Some(&LogKind::DroppedNotice));
        assert!(!logs.iter().any(|message| message == "must-not-appear-after-drop"));
    }
}
