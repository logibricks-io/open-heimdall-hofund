//! D-253 and D-259: a local resume reply and a client cancel.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use hofund_plugin_api::{AccountId, ContentPart, Message, Request, Role};
use serde_json::json;

use super::{Bridge, CancelGuard, CutGate, TurnStop};
use crate::bridge::cut::{self, LOCAL_RESUME_REPLY};

const OWNER: &str = "a-00000000000000aa";
const OTHER: &str = "a-00000000000000bb";

fn account(id: &'static str) -> AccountId {
    AccountId::from_static(id)
}

fn owner() -> AccountId {
    AccountId::from_static(OWNER)
}

fn text(role: &str, body: &str) -> serde_json::Value {
    json!({"role": role, "content": [{"type": "text", "text": body}]})
}

fn base_request(messages: Vec<serde_json::Value>) -> Request {
    let mut messages = messages;
    messages.insert(0, text("system", "rules"));
    serde_json::from_value(json!({
        "model": "claude-sonnet-4-5",
        "messages": messages,
        "tools": [{"name": "read", "input_schema": {"type": "object"}}]
    }))
    .expect("request")
}

fn first() -> Request {
    base_request(vec![text("user", "original ask")])
}

fn after_local(cancelled: bool, next: &str) -> Request {
    let mut messages = vec![text("user", "original ask"), text("assistant", "ok")];
    if cancelled {
        messages.push(text("user", "cancelled ask"));
    }
    messages.push(text("assistant", LOCAL_RESUME_REPLY));
    messages.push(text("user", next));
    base_request(messages)
}

fn park(bridge: &Bridge) -> u64 {
    let id = bridge.park_run(owner(), vec!["read".into()]);
    bridge.ensure_session_run(id, &owner(), vec!["read".into()]);
    id
}

fn reply_line(session: &str, id: &str, stop: &str) -> String {
    format!(
        "{{\"type\":\"assistant\",\"uuid\":\"line-1\",\"sessionId\":\"{session}\",\"message\":{{\"id\":\"{id}\",\"role\":\"assistant\",\"stop_reason\":\"{stop}\",\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}\n"
    )
}

fn write_session(dir: &std::path::Path, session: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join(format!("{session}.jsonl"));
    std::fs::write(&path, body).expect("transcript");
    path
}

fn idle_with_reply(bridge: &Bridge, dir: &std::path::Path) -> u64 {
    let run = park(bridge);
    let request = first();
    bridge.note_completed_reply(run, "msg_1");
    bridge.heard(run, &request.messages);
    write_session(dir, "sess_1", &reply_line("sess_1", "msg_1", "end_turn"));
    bridge.set_session(run, "sess_1", vec![dir.to_path_buf()]);
    bridge.ended(run, &owner(), &request, "ok", TurnStop::Stop, true);
    run
}

type PrefixCase = (&'static str, fn(&mut Request, &mut AccountId));

fn leave_request(_: &mut Request, _: &mut AccountId) {}

fn other_account(_: &mut Request, owner: &mut AccountId) {
    *owner = account(OTHER);
}

fn other_model(request: &mut Request, _: &mut AccountId) {
    request.model = "other".into();
}

fn other_system(request: &mut Request, _: &mut AccountId) {
    request.messages[0].content = vec![ContentPart::text("other")];
}

fn other_tool_schema(request: &mut Request, _: &mut AccountId) {
    request.tools[0].input_schema = json!({"type": "object", "required": ["path"]});
}

fn other_settings(request: &mut Request, _: &mut AccountId) {
    request.extensions.insert("hofund_think_off", json!(true));
}

fn changed_prefix(request: &mut Request, _: &mut AccountId) {
    request.messages[1].content = vec![ContentPart::text("other ask")];
}

fn another_reply(request: &mut Request, _: &mut AccountId) {
    request.messages[2].content = vec![ContentPart::text("from elsewhere")];
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| {
            message.content.iter().filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.clone()),
                _ => None,
            })
        })
        .collect()
}

#[test]
fn d253_exact_miss_resumes_past_the_local_reply() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let follow = after_local(true, "more after resume");
    let (local_key, _) = super::next_turn(OWNER, &follow).expect("local reply is a turn");
    assert!(bridge.shelved(&local_key).is_none());
    let found = bridge.resume(&follow, &owner()).expect("prefix resume");
    assert_eq!(found.run_id, run);
    let said = texts(&found.since);
    assert!(said.iter().any(|text| text == "cancelled ask"));
    assert!(said.iter().any(|text| text == LOCAL_RESUME_REPLY));
    assert!(said.iter().any(|text| text == "more after resume"));
    assert!(!said.iter().any(|text| text == "original ask"));
    assert!(!bridge.is_idle(run));
}

#[test]
fn d253_exact_miss_unshelves_past_the_local_reply() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    idle_with_reply(&bridge, dir.path());
    assert!(bridge.evict_idle_for_room());
    let follow = after_local(false, "more");
    let (saved, since) = bridge.unshelve(&follow, &owner()).expect("unshelve");
    assert_eq!(saved.session, "sess_1");
    let said = texts(&since);
    assert!(said.iter().any(|text| text == LOCAL_RESUME_REPLY));
    assert!(said.iter().any(|text| text == "more"));
    assert!(!said.iter().any(|text| text == "original ask"));
}

#[test]
fn d253_prefix_requires_the_same_conversation() {
    let dir = tempfile::tempdir().expect("dir");
    let follow = after_local(true, "more");
    let cases: &[PrefixCase] = &[
        ("unchanged", leave_request),
        ("other account", other_account),
        ("other model", other_model),
        ("other system", other_system),
        ("other tool schema", other_tool_schema),
        ("other settings", other_settings),
        ("changed prefix", changed_prefix),
        ("another reply", another_reply),
    ];
    for &(name, change) in cases {
        let bridge = Bridge::new();
        idle_with_reply(&bridge, dir.path());
        assert!(bridge.evict_idle_for_room(), "{name}");
        let mut request = follow.clone();
        let mut who = owner();
        change(&mut request, &mut who);
        let found = bridge.unshelve(&request, &who);
        if name == "unchanged" {
            let (_, since) = found.expect(name);
            assert!(
                texts(&since).iter().any(|text| text == "cancelled ask"),
                "{name}"
            );
        } else {
            assert!(found.is_none(), "{name} shared a run");
        }
    }
}

#[test]
fn d253_unmatched_result_is_not_a_prefix() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    idle_with_reply(&bridge, dir.path());
    let mut request = after_local(false, "more");
    request.messages[3] = serde_json::from_value(json!({
        "role": "user",
        "content": [{"type": "tool_result", "id": "missing", "content": [{"type": "text", "text": "result"}]}]
    }))
    .expect("result");
    assert!(cut::past_local_reply(OWNER, &request).is_none());
    assert!(bridge.resume(&request, &owner()).is_none());
}

#[test]
fn d253_turn_from_keeps_the_turn_taken() {
    let mut messages = vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "cancelled ask"),
        text("assistant", LOCAL_RESUME_REPLY),
        text("user", "more"),
    ];
    messages.push(json!({
        "role": "assistant",
        "content": [{
            "type": "tool_call",
            "id": "c1",
            "name": "read",
            "arguments": {"form": "json", "value": {}}
        }]
    }));
    messages.push(json!({
        "role": "user",
        "content": [{"type": "tool_result", "id": "c1", "content": [{"type": "text", "text": "result"}]}]
    }));
    let request = base_request(messages);
    let assistant_at = request
        .messages
        .iter()
        .position(|message| message.role == Role::Assistant)
        .expect("reply");
    let prefix = &request.messages[..=assistant_at];
    let took = super::turn_key(OWNER, &request, prefix);
    let (key, since) = cut::turn_from(OWNER, &request, &took).expect("turn");
    assert_eq!(key, took);
    assert!(texts(since).iter().any(|text| text == "cancelled ask"));
    let (next, next_since) = super::next_turn(OWNER, &request).expect("next");
    assert_ne!(next, took);
    for other in ["", next.as_str(), "not-a-key"] {
        let (key, since) = cut::turn_from(OWNER, &request, other).expect("fallback");
        assert_eq!(key, next);
        assert_eq!(since.len(), next_since.len());
    }
    let plain = first();
    assert!(cut::past_local_reply(OWNER, &plain).is_none());
}

#[test]
fn d253_limit_refusal_keeps_the_cancelled_ask() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let follow = after_local(true, "and run the tests");
    let resumed = bridge.resume(&follow, &owner()).expect("resume");
    assert_eq!(resumed.run_id, run);
    bridge.heard(run, &follow.messages);
    bridge.keep_for_reset(run, &follow, &owner());
    let (saved, since) = bridge.unshelve(&follow, &owner()).expect("kept");
    assert_ne!(saved.session, "sess_1");
    assert!(texts(&since).iter().any(|text| text == "cancelled ask"));
    assert!(texts(&since).iter().any(|text| text == LOCAL_RESUME_REPLY));
    let copy =
        std::fs::read_to_string(dir.path().join(format!("{}.jsonl", saved.session))).expect("copy");
    assert!(copy.contains("msg_1"));
    assert!(!copy.contains("cancelled ask"));
    assert!(copy.contains(&format!("\"sessionId\":\"{}\"", saved.session)));
    assert!(!copy.contains("\"sessionId\":\"sess_1\""));
}

#[test]
fn d253_busy_retry_keeps_the_cancelled_ask() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let follow = after_local(true, "and run the tests");
    bridge.resume(&follow, &owner()).expect("resume");
    bridge.heard(run, &follow.messages);
    let (saved, since) = bridge.retry_branch(&follow, &owner()).expect("retry");
    assert_ne!(saved.session, "sess_1");
    assert!(texts(&since).iter().any(|text| text == "cancelled ask"));
    assert!(!bridge.is_idle(run));
}

#[test]
fn d253_client_cancel_shelves_the_cut_at_end_turn() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let cancelled = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    bridge.resume(&cancelled, &owner()).expect("resume");
    bridge.client_cancelled(run, &cancelled, &owner());
    assert!(!bridge.is_idle(run));
    let next = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "compact now"),
    ]);
    let (saved, since) = bridge.unshelve(&next, &owner()).expect("replacement");
    assert_eq!(texts(&since), vec!["compact now".to_string()]);
    let copy =
        std::fs::read_to_string(dir.path().join(format!("{}.jsonl", saved.session))).expect("copy");
    assert!(copy.contains("end_turn"));
    assert!(!copy.contains("CANCEL ME"));
}

#[test]
fn d253_client_cancel_from_a_saved_session_cuts_at_from_len() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let body = reply_line("sess_saved", "msg_0", "end_turn");
    let path = write_session(dir.path(), "sess_saved", &body);
    let run = park(&bridge);
    let request = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    let (key, since) = super::next_turn(OWNER, &request).expect("turn");
    let saved = super::SavedSession {
        session: "sess_saved".into(),
        sessions: vec![dir.path().to_path_buf()],
        owner: OWNER.into(),
        conv: String::new(),
        key: key.clone(),
        at: std::time::Instant::now(),
        copy: false,
    };
    bridge.note_started_from(run, &saved, &request, since.len());
    std::fs::write(
        &path,
        format!("{body}{{\"type\":\"user\",\"message\":{{\"content\":\"CANCEL ME\"}}}}\n"),
    )
    .expect("append");
    bridge.client_cancelled(run, &request, &owner());
    let next = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "only this turn"),
    ]);
    let (cut, told) = bridge.unshelve(&next, &owner()).expect("cut");
    assert_eq!(texts(&told), vec!["only this turn".to_string()]);
    let copy = std::fs::read(dir.path().join(format!("{}.jsonl", cut.session))).expect("copy");
    assert!(
        !copy
            .windows(b"CANCEL ME".len())
            .any(|window| window == b"CANCEL ME")
    );
    assert!(
        copy.windows(b"end_turn".len())
            .any(|window| window == b"end_turn")
    );
    assert!(
        copy.windows(cut.session.len())
            .any(|window| window == cut.session.as_bytes())
    );
}

#[test]
fn d253_client_cancel_of_a_new_run_keeps_nothing() {
    let bridge = Bridge::new();
    let run = park(&bridge);
    let request = first();
    bridge.begin_turn(run, &request, Some(true));
    bridge.client_cancelled(run, &request, &owner());
    assert!(
        bridge
            .unshelve(&after_local(false, "more"), &owner())
            .is_none()
    );
}

#[test]
fn d253_incomplete_reply_is_not_a_cut() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = park(&bridge);
    write_session(
        dir.path(),
        "sess_1",
        &reply_line("sess_1", "msg_1", "tool_use"),
    );
    bridge.set_session(run, "sess_1", vec![dir.path().to_path_buf()]);
    bridge.note_completed_reply(run, "msg_1");
    let request = first();
    bridge.ended(run, &owner(), &request, "ok", TurnStop::Stop, true);
    let follow = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    bridge.resume(&follow, &owner()).expect("resume");
    bridge.client_cancelled(run, &follow, &owner());
    assert!(bridge.unshelve(&follow, &owner()).is_none());
}

#[test]
fn d253_cache_marks_handed_stopping_and_wsl_keep_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let follow = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    for case in ["marks", "handed", "stopping", "wsl"] {
        let bridge = Bridge::new();
        let run = idle_with_reply(&bridge, dir.path());
        bridge.resume(&follow, &owner()).expect(case);
        match case {
            "marks" => {
                let mut marked = follow.clone();
                marked
                    .extensions
                    .insert("hofund_client_cache_marks", json!(true));
                bridge.begin_turn(run, &marked, None);
                bridge.client_cancelled(run, &follow, &owner());
            }
            "handed" => {
                bridge.account_handed_over(&owner());
                bridge.client_cancelled(run, &follow, &owner());
            }
            "stopping" => {
                bridge.mark_stopping();
                bridge.client_cancelled(run, &follow, &owner());
            }
            "wsl" => {
                bridge.begin_turn(run, &follow, Some(false));
                bridge.client_cancelled(run, &follow, &owner());
            }
            _ => unreachable!(),
        }
        assert!(bridge.unshelve(&follow, &owner()).is_none(), "{case}");
    }
}

#[test]
fn d253_same_key_waits_and_the_client_that_goes_is_499() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let follow = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    bridge.resume(&follow, &owner()).expect("resume");
    let key = super::next_turn(OWNER, &follow).expect("key").0;
    let gate = Arc::new(CutGate::new());
    bridge.install_cut_gate(Arc::clone(&gate));
    let gone_a = Arc::new(AtomicBool::new(false));
    let ticket = bridge
        .watch_cancel_blocking(&key, Arc::clone(&gone_a))
        .expect("register")
        .expect("ticket");
    let cutter = {
        let bridge = bridge.clone();
        let follow = follow.clone();
        let gone_a = Arc::clone(&gone_a);
        thread::spawn(move || {
            gone_a.store(true, Ordering::Release);
            bridge.client_cancelled(run, &follow, &owner());
        })
    };
    gate.wait_entered();
    let gone_b = Arc::new(AtomicBool::new(false));
    let waiter = {
        let bridge = bridge.clone();
        let key = key.clone();
        let gone_b = Arc::clone(&gone_b);
        thread::spawn(move || bridge.watch_cancel_blocking(&key, gone_b))
    };
    bridge.wait_for_cancel_waiter();
    assert_eq!(bridge.cancel_waiters(), 1);
    gone_b.store(true, Ordering::Release);
    bridge.wake_cancels();
    let error = waiter.join().expect("waiter").expect_err("closed");
    assert_eq!(error.status(), Some(499));
    gate.release();
    cutter.join().expect("cutter");
    bridge.finish_cancel(ticket);
    let next = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "replacement"),
    ]);
    let (_, since) = bridge
        .unshelve(&next, &owner())
        .expect("shelved after the cut");
    assert_eq!(texts(&since), vec!["replacement".to_string()]);
}

#[test]
fn d253_dropped_turn_guard_shelves_the_cut() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let follow = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    bridge.resume(&follow, &owner()).expect("resume");
    let key = super::next_turn(OWNER, &follow).expect("key").0;
    let gone = Arc::new(AtomicBool::new(false));
    let ticket = bridge
        .watch_cancel_blocking(&key, Arc::clone(&gone))
        .expect("register");
    let guard = CancelGuard::arm(
        bridge.clone(),
        follow.clone(),
        owner(),
        ticket,
        Arc::clone(&gone),
    );
    guard.bind(run);
    drop(guard);
    assert!(gone.load(Ordering::Acquire));
    let next = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "after drop"),
    ]);
    let (_, since) = bridge.unshelve(&next, &owner()).expect("drop shelved");
    assert_eq!(texts(&since), vec!["after drop".to_string()]);
    let kept = CancelGuard::arm(
        bridge.clone(),
        follow,
        owner(),
        None,
        Arc::new(AtomicBool::new(false)),
    );
    kept.defuse();
    drop(kept);
}

#[test]
fn d253_async_same_key_watch_returns_499_when_its_client_goes() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let follow = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    bridge.resume(&follow, &owner()).expect("resume");
    let key = super::next_turn(OWNER, &follow).expect("key").0;
    let gate = Arc::new(CutGate::new());
    bridge.install_cut_gate(Arc::clone(&gate));
    let gone_a = Arc::new(AtomicBool::new(false));
    let ticket = bridge
        .watch_cancel_blocking(&key, Arc::clone(&gone_a))
        .expect("register")
        .expect("ticket");
    let cutter = {
        let bridge = bridge.clone();
        let follow = follow.clone();
        let gone_a = Arc::clone(&gone_a);
        thread::spawn(move || {
            gone_a.store(true, Ordering::Release);
            bridge.client_cancelled(run, &follow, &owner());
        })
    };
    gate.wait_entered();
    let gone_b = Arc::new(AtomicBool::new(false));
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut waiting = std::pin::pin!(bridge.watch_cancel(&key, Arc::clone(&gone_b)));
    assert!(waiting.as_mut().poll(&mut cx).is_pending());
    gone_b.store(true, Ordering::Release);
    bridge.wake_cancels();
    let error = match waiting.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => result.expect_err("closed"),
        std::task::Poll::Pending => panic!("the watch stayed pending after the client went"),
    };
    assert_eq!(error.status(), Some(499));
    gate.release();
    cutter.join().expect("cutter");
    bridge.finish_cancel(ticket);
}

#[test]
fn d253_cancel_finish_between_pending_check_and_wait_is_not_lost() {
    let bridge = Bridge::new();
    let key = "same-turn";
    let gone_a = Arc::new(AtomicBool::new(false));
    let ticket = bridge
        .watch_cancel_blocking(key, Arc::clone(&gone_a))
        .expect("register")
        .expect("ticket");
    gone_a.store(true, Ordering::Release);
    let gate = Arc::new(CutGate::new());
    bridge.install_watch_gate(Arc::clone(&gate));
    let watching = {
        let bridge = bridge.clone();
        let key = key.to_owned();
        thread::spawn(move || {
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let gone = Arc::new(AtomicBool::new(false));
            let mut waiting = std::pin::pin!(bridge.watch_cancel(&key, gone));
            waiting.as_mut().poll(&mut cx)
        })
    };
    gate.wait_entered();
    bridge.finish_cancel(ticket);
    gate.release();
    let result = watching.join().expect("watch poll");
    let ticket = match result {
        std::task::Poll::Ready(result) => result
            .expect("registration")
            .expect("a new ticket after the completed cut"),
        std::task::Poll::Pending => panic!("the wait missed the completed cancel"),
    };
    bridge.finish_cancel(ticket);
}

#[test]
fn d253_retire_deletes_a_shelved_cut_file() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    let cancelled = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "CANCEL ME"),
    ]);
    bridge.resume(&cancelled, &owner()).expect("resume");
    bridge.client_cancelled(run, &cancelled, &owner());
    let key = super::next_turn(OWNER, &cancelled).expect("key").0;
    let saved = bridge.shelved(&key).expect("cut");
    let path = dir.path().join(format!("{}.jsonl", saved.session));
    assert!(path.is_file());
    let next = base_request(vec![
        text("user", "original ask"),
        text("assistant", "ok"),
        text("user", "replacement"),
    ]);
    bridge.retire(&owner(), &next);
    assert!(bridge.shelved(&key).is_none());
    assert!(!path.exists(), "retire left the cut transcript behind");
}

#[test]
fn d253_shelve_run_trim_deletes_the_oldest_cut_file() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let now = std::time::Instant::now();
    for index in 0..super::SHELF_MOST {
        let session = format!("copy_{index}");
        write_session(dir.path(), &session, "cut\n");
        bridge.insert_shelf(super::SavedSession {
            session,
            sessions: vec![dir.path().to_path_buf()],
            owner: OWNER.into(),
            conv: format!("conv_{index}"),
            key: format!("key_{index}"),
            at: now + std::time::Duration::from_secs(index as u64),
            copy: true,
        });
    }
    idle_with_reply(&bridge, dir.path());
    assert!(bridge.evict_idle_for_room());
    assert!(
        !dir.path().join("copy_0.jsonl").exists(),
        "shelf trim left the oldest cut transcript behind"
    );
    assert!(dir.path().join("copy_1.jsonl").is_file());
}

#[test]
fn d253_cancel_rechecks_handed_and_stopping_after_the_copy() {
    for case in ["clear", "handed", "stopping"] {
        let bridge = Bridge::new();
        let dir = tempfile::tempdir().expect("dir");
        let run = idle_with_reply(&bridge, dir.path());
        let cancelled = base_request(vec![
            text("user", "original ask"),
            text("assistant", "ok"),
            text("user", "CANCEL ME"),
        ]);
        bridge.resume(&cancelled, &owner()).expect("resume");
        let gate = Arc::new(CutGate::new());
        bridge.install_cut_gate(Arc::clone(&gate));
        let cutting = {
            let bridge = bridge.clone();
            let cancelled = cancelled.clone();
            thread::spawn(move || bridge.client_cancelled(run, &cancelled, &owner()))
        };
        gate.wait_entered();
        match case {
            "handed" => bridge.account_handed_over(&owner()),
            "stopping" => bridge.mark_stopping(),
            _ => {}
        }
        gate.release();
        cutting.join().expect("cut");
        let next = base_request(vec![
            text("user", "original ask"),
            text("assistant", "ok"),
            text("user", "replacement"),
        ]);
        let kept = bridge.unshelve(&next, &owner());
        if case == "clear" {
            assert!(kept.is_some(), "a still-current run kept no cut");
        } else {
            assert!(kept.is_none(), "{case} published a cut after the flag");
        }
    }
}

#[test]
fn d253_session_discovery_miss_keeps_known_paths() {
    let bridge = Bridge::new();
    let dir = tempfile::tempdir().expect("dir");
    let run = idle_with_reply(&bridge, dir.path());
    bridge.set_session(run, "sess_1", Vec::new());
    let saved = bridge.saved_from_run(run).expect("idle session");
    assert_eq!(saved.sessions, vec![dir.path().to_path_buf()]);
}

#[test]
fn d253_completed_reply_id_needs_end_turn() {
    let stdout = "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"id\":\"msg_text\"}}}\n{\"type\":\"stream_event\",\"event\":{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}}\n";
    assert_eq!(cut::completed_reply_id(stdout).as_deref(), Some("msg_text"));
    assert!(cut::completed_reply_id("{\"type\":\"stream_event\",\"event\":{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}}").is_none());
}
