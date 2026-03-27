#![deny(unsafe_code)]
#![deny(clippy::all)]
#![warn(missing_docs)]

//! Telegram Bot uplink capsule for Astrid OS.
//!
//! Bridges the Telegram Bot API to the kernel IPC bus. Polls Telegram for
//! updates, publishes user input as `user.v1.prompt`, and renders agent
//! responses, approvals, and elicitations back to Telegram chats.

mod format;
mod telegram;
mod types;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use astrid_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Minimum interval between Telegram message edits (rate-limit guard).
const EDIT_THROTTLE: Duration = Duration::from_millis(500);

/// Telegram long-poll timeout (seconds). Kept short so we can interleave IPC
/// event processing between polls.
const POLL_TIMEOUT: u32 = 1;

/// KV key for the last processed Telegram update offset.
const KV_OFFSET: &str = "tg.offset";

/// Persisted session mapping entry.
#[derive(Serialize, Deserialize)]
struct SessionEntry {
    session_id: String,
}

/// Transient per-chat state for the current turn.
struct TurnState {
    /// Telegram message ID being edited with streaming text.
    msg_id: i64,
    /// Accumulated response text (markdown).
    text_buffer: String,
    /// Last time we edited the Telegram message.
    last_edit: Instant,
    /// Whether the current message has been finalized (e.g. before a tool).
    finalized: bool,
}

/// Pending approval waiting for a callback button press.
struct PendingApproval {
    chat_id: i64,
}

/// Telegram Bot uplink capsule.
#[derive(Default)]
pub struct TelegramBot;

#[capsule]
impl TelegramBot {
    /// Main run loop: poll Telegram and IPC in alternation.
    #[astrid::run]
    fn run(&self) -> Result<(), SysError> {
        // ── Config ──────────────────────────────────────────────────────
        let bot_token = env::var("bot_token")
            .map_err(|_| SysError::ApiError("bot_token not configured".into()))?;
        let allowed_users = parse_allowed_users(&env::var("allowed_user_ids").unwrap_or_default());

        if allowed_users.is_empty() {
            let _ = log::warn(
                "Telegram bot starting with NO user restrictions — \
                 any Telegram user can interact with the agent. \
                 Set allowed_user_ids to restrict access.",
            );
        }

        // ── Register uplink ─────────────────────────────────────────────
        let _ = uplink::register("telegram", "telegram", "interactive");

        // ── Subscribe to IPC topics ─────────────────────────────────────
        let topics = [
            "agent.v1.response",
            "agent.v1.stream.delta",
            "astrid.v1.approval",
            "astrid.v1.elicit.*",
            "astrid.v1.response.*",
        ];
        let sub_handles: Vec<_> = topics
            .iter()
            .map(|t| ipc::subscribe(t).map_err(|e| SysError::ApiError(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;

        let _ = runtime::signal_ready();

        // ── State ───────────────────────────────────────────────────────
        let mut offset: i64 = kv::get_json(KV_OFFSET).unwrap_or(0);
        let mut sessions: HashMap<i64, String> = load_sessions();
        let mut session_to_chat: HashMap<String, i64> = sessions
            .iter()
            .map(|(&chat_id, sid)| (sid.clone(), chat_id))
            .collect();
        let mut turns: HashMap<i64, TurnState> = HashMap::new();
        let mut pending_approvals: HashMap<String, PendingApproval> = HashMap::new();

        let _ = log::info("Telegram bot started");

        // ── Main loop ───────────────────────────────────────────────────
        loop {
            // Phase A: poll Telegram for new updates.
            match telegram::get_updates(&bot_token, offset, POLL_TIMEOUT) {
                Ok(updates) => {
                    for update in updates {
                        offset = update.update_id + 1;
                        handle_telegram_update(
                            &bot_token,
                            &allowed_users,
                            &update,
                            &mut sessions,
                            &mut session_to_chat,
                            &mut turns,
                            &mut pending_approvals,
                        );
                    }
                    let _ = kv::set_json(KV_OFFSET, &offset);
                }
                Err(e) => {
                    let _ = log::warn(format!("Telegram poll error: {e:?}"));
                    std::thread::sleep(Duration::from_secs(2));
                }
            }

            // Phase B: poll IPC events and push to Telegram.
            for handle in &sub_handles {
                match ipc::poll_bytes(handle) {
                    Ok(bytes) if !bytes.is_empty() => {
                        handle_ipc_poll(
                            &bot_token,
                            &bytes,
                            &session_to_chat,
                            &sessions,
                            &mut turns,
                            &mut pending_approvals,
                        );
                    }
                    Err(e) => {
                        let _ = log::error(format!("IPC poll error: {e:?}"));
                    }
                    _ => {}
                }
            }
        }
    }
}

// ── Telegram Update Handling ────────────────────────────────────────────────

fn handle_telegram_update(
    token: &str,
    allowed_users: &[i64],
    update: &types::Update,
    sessions: &mut HashMap<i64, String>,
    session_to_chat: &mut HashMap<String, i64>,
    turns: &mut HashMap<i64, TurnState>,
    pending_approvals: &mut HashMap<String, PendingApproval>,
) {
    if let Some(msg) = &update.message {
        handle_message(token, allowed_users, msg, sessions, session_to_chat, turns);
    }
    if let Some(cb) = &update.callback_query {
        handle_callback(token, allowed_users, cb, sessions, pending_approvals);
    }
}

fn handle_message(
    token: &str,
    allowed_users: &[i64],
    msg: &types::TgMessage,
    sessions: &mut HashMap<i64, String>,
    session_to_chat: &mut HashMap<String, i64>,
    turns: &mut HashMap<i64, TurnState>,
) {
    let chat_id = msg.chat.id;
    let Some(text) = &msg.text else { return };

    // Access control.
    if !is_user_allowed(allowed_users, msg.from.as_ref().map(|u| u.id)) {
        let _ = telegram::send_message(token, chat_id, "Not authorized.", None, None);
        return;
    }

    // Bot commands.
    if text.starts_with('/') {
        handle_command(token, chat_id, text, sessions, session_to_chat, turns);
        return;
    }

    // Check if a turn is already in progress.
    if turns.contains_key(&chat_id) {
        let _ = telegram::send_message(
            token,
            chat_id,
            "A turn is already in progress. Please wait or /cancel.",
            None,
            None,
        );
        return;
    }

    // Ensure session exists.
    let session_id = sessions
        .entry(chat_id)
        .or_insert_with(|| {
            let sid = new_session_id(chat_id);
            session_to_chat.insert(sid.clone(), chat_id);
            save_session(chat_id, &sid);
            sid
        })
        .clone();

    // Send "Thinking..." placeholder.
    let placeholder = match telegram::send_message(token, chat_id, "Thinking...", None, None) {
        Ok(m) => m,
        Err(e) => {
            let _ = log::warn(format!("Failed to send placeholder: {e:?}"));
            return;
        }
    };

    let _ = telegram::send_typing(token, chat_id);

    // Start turn tracking.
    turns.insert(
        chat_id,
        TurnState {
            msg_id: placeholder.message_id,
            text_buffer: String::new(),
            last_edit: Instant::now()
                .checked_sub(EDIT_THROTTLE)
                .unwrap_or_else(Instant::now),
            finalized: false,
        },
    );

    // Publish user input to the IPC bus.
    let payload = serde_json::json!({
        "type": "user_input",
        "text": text,
        "session_id": session_id,
    });
    if let Err(e) = ipc::publish_json("user.v1.prompt", &payload) {
        let _ = log::error(format!("Failed to publish user input: {e:?}"));
        let _ = telegram::edit_message_text(
            token,
            chat_id,
            placeholder.message_id,
            "Failed to send message to agent.",
            None,
        );
        turns.remove(&chat_id);
    }
}

fn handle_command(
    token: &str,
    chat_id: i64,
    text: &str,
    sessions: &mut HashMap<i64, String>,
    session_to_chat: &mut HashMap<String, i64>,
    turns: &mut HashMap<i64, TurnState>,
) {
    let cmd = text.split_whitespace().next().unwrap_or("");
    match cmd {
        "/start" | "/help" => {
            let help = "<b>Astrid Telegram Bot</b>\n\n\
                        Send any text message to interact with the agent.\n\n\
                        <b>Commands:</b>\n\
                        /start — Welcome message\n\
                        /help — This help text\n\
                        /reset — End session and start fresh\n\
                        /cancel — Cancel the current turn";
            let _ = telegram::send_message(token, chat_id, help, Some("HTML"), None);
        }
        "/reset" => {
            if let Some(sid) = sessions.remove(&chat_id) {
                session_to_chat.remove(&sid);
                delete_session(chat_id);
            }
            turns.remove(&chat_id);
            let _ = telegram::send_message(token, chat_id, "Session reset.", None, None);
        }
        "/cancel" => {
            if turns.remove(&chat_id).is_some() {
                let _ = telegram::send_message(token, chat_id, "Turn cancelled.", None, None);
            } else {
                let _ = telegram::send_message(token, chat_id, "No turn in progress.", None, None);
            }
        }
        _ => {
            let _ =
                telegram::send_message(token, chat_id, "Unknown command. Try /help.", None, None);
        }
    }
}

fn handle_callback(
    token: &str,
    allowed_users: &[i64],
    cb: &types::CallbackQuery,
    sessions: &HashMap<i64, String>,
    pending_approvals: &mut HashMap<String, PendingApproval>,
) {
    // Access control.
    if !is_user_allowed(allowed_users, Some(cb.from.id)) {
        let _ = telegram::answer_callback_query(token, &cb.id, Some("Not authorized"));
        return;
    }

    let Some(data) = &cb.data else {
        let _ = telegram::answer_callback_query(token, &cb.id, None);
        return;
    };

    // Parse callback data: "apr:{request_id}:{option}" or "eli:{request_id}:{value}"
    let parts: Vec<&str> = data.splitn(3, ':').collect();
    if parts.len() < 3 {
        let _ = telegram::answer_callback_query(token, &cb.id, Some("Unknown action"));
        return;
    }

    match parts[0] {
        "apr" => {
            let request_id = parts[1];
            let decision = parts[2];
            if let Some(approval) = pending_approvals.remove(request_id) {
                let payload = serde_json::json!({
                    "type": "approval_response",
                    "request_id": request_id,
                    "decision": decision,
                });
                let topic = format!("astrid.v1.approval.response.{request_id}");
                let _ = ipc::publish_json(&topic, &payload);
                let _ = telegram::answer_callback_query(
                    token,
                    &cb.id,
                    Some(&format!("Approved: {decision}")),
                );

                // Edit the approval message to show the decision.
                if let Some(msg) = &cb.message {
                    let _ = telegram::edit_message_text(
                        token,
                        approval.chat_id,
                        msg.message_id,
                        &format!("Approval: <b>{decision}</b>"),
                        Some("HTML"),
                    );
                }
            } else {
                let _ = telegram::answer_callback_query(token, &cb.id, Some("Approval expired"));
            }
        }
        "eli" => {
            let request_id = parts[1];
            let value = parts[2];
            let chat_id = cb.message.as_ref().map(|m| m.chat.id);

            // Find session for this chat.
            let session_id = chat_id.and_then(|cid| sessions.get(&cid));

            if session_id.is_some() {
                let payload = serde_json::json!({
                    "type": "elicit_response",
                    "request_id": request_id,
                    "value": value,
                });
                let topic = format!("astrid.v1.elicit.response.{request_id}");
                let _ = ipc::publish_json(&topic, &payload);
                let _ = telegram::answer_callback_query(
                    token,
                    &cb.id,
                    Some(&format!("Selected: {value}")),
                );
            } else {
                let _ = telegram::answer_callback_query(token, &cb.id, Some("No active session"));
            }
        }
        _ => {
            let _ = telegram::answer_callback_query(token, &cb.id, Some("Unknown action"));
        }
    }
}

// ── IPC Event Handling ──────────────────────────────────────────────────────

fn handle_ipc_poll(
    token: &str,
    poll_bytes: &[u8],
    session_to_chat: &HashMap<String, i64>,
    sessions: &HashMap<i64, String>,
    turns: &mut HashMap<i64, TurnState>,
    pending_approvals: &mut HashMap<String, PendingApproval>,
) {
    let envelope: Value = match serde_json::from_slice(poll_bytes) {
        Ok(v) => v,
        Err(_) => return,
    };

    let Some(messages) = envelope.get("messages").and_then(|m| m.as_array()) else {
        return;
    };

    for msg in messages {
        let topic = msg.get("topic").and_then(|t| t.as_str()).unwrap_or("");
        let Some(payload) = msg.get("payload") else {
            continue;
        };

        handle_ipc_event(
            token,
            topic,
            payload,
            session_to_chat,
            sessions,
            turns,
            pending_approvals,
        );
    }
}

fn handle_ipc_event(
    token: &str,
    topic: &str,
    payload: &Value,
    session_to_chat: &HashMap<String, i64>,
    sessions: &HashMap<i64, String>,
    turns: &mut HashMap<i64, TurnState>,
    pending_approvals: &mut HashMap<String, PendingApproval>,
) {
    let event_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "agent_response" => {
            let session_id = payload
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let text = payload.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let is_final = payload
                .get("is_final")
                .and_then(|f| f.as_bool())
                .unwrap_or(false);

            let Some(&chat_id) = session_to_chat.get(session_id) else {
                return;
            };

            if is_final {
                handle_final_response(token, chat_id, text, turns);
            } else {
                handle_stream_delta(token, chat_id, text, turns);
            }
        }

        "approval_required" => {
            let request_id = payload
                .get("request_id")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let action = payload
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("unknown");
            let resource = payload
                .get("resource")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let reason = payload.get("reason").and_then(|r| r.as_str()).unwrap_or("");

            // Find which chat this approval belongs to by checking active turns.
            let chat_id = find_chat_for_event(session_to_chat, sessions, turns);
            let Some(chat_id) = chat_id else { return };

            handle_approval_request(
                token,
                chat_id,
                request_id,
                action,
                resource,
                reason,
                turns,
                pending_approvals,
            );
        }

        "elicit_request" => {
            let request_id = payload
                .get("request_id")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let field = payload.get("field");

            let chat_id = find_chat_for_event(session_to_chat, sessions, turns);
            let Some(chat_id) = chat_id else { return };

            handle_elicitation_request(token, chat_id, request_id, field);
        }

        // Catch-all for stream deltas that use a different topic pattern.
        _ if topic.starts_with("agent.v1.stream") => {
            let session_id = payload
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let text = payload.get("text").and_then(|t| t.as_str()).unwrap_or("");

            if let Some(&chat_id) = session_to_chat.get(session_id) {
                handle_stream_delta(token, chat_id, text, turns);
            }
        }

        _ => {}
    }
}

fn handle_stream_delta(token: &str, chat_id: i64, text: &str, turns: &mut HashMap<i64, TurnState>) {
    if text.is_empty() {
        return;
    }

    let Some(turn) = turns.get_mut(&chat_id) else {
        return;
    };

    turn.text_buffer.push_str(text);

    if turn.last_edit.elapsed() >= EDIT_THROTTLE && !turn.text_buffer.is_empty() {
        let html = format::md_to_telegram_html(&turn.text_buffer);
        let display = format::truncate_for_edit(&html);

        if turn.finalized {
            // Previous content was finalized; send a new message.
            if let Ok(msg) = telegram::send_message(token, chat_id, &display, Some("HTML"), None) {
                turn.msg_id = msg.message_id;
                turn.finalized = false;
            }
        } else {
            let _ =
                telegram::edit_message_text(token, chat_id, turn.msg_id, &display, Some("HTML"));
        }
        turn.last_edit = Instant::now();
    }
}

fn handle_final_response(
    token: &str,
    chat_id: i64,
    text: &str,
    turns: &mut HashMap<i64, TurnState>,
) {
    if let Some(turn) = turns.remove(&chat_id) {
        // Use the final text if provided, otherwise finalize the buffer.
        let final_text = if text.is_empty() {
            &turn.text_buffer
        } else {
            text
        };

        if !final_text.is_empty() {
            let html = format::md_to_telegram_html(final_text);
            let chunks = format::chunk_html(&html, 4000);

            if let Some((first, rest)) = chunks.split_first() {
                if turn.finalized {
                    // Send as new message.
                    let _ =
                        telegram::send_message(token, chat_id, first, Some("HTML"), None);
                    for chunk in rest {
                        let _ =
                            telegram::send_message(token, chat_id, chunk, Some("HTML"), None);
                    }
                } else {
                    // Edit the existing message with the final text.
                    let _ = telegram::edit_message_text(
                        token,
                        chat_id,
                        turn.msg_id,
                        first,
                        Some("HTML"),
                    );
                    for chunk in rest {
                        let _ = telegram::send_message(token, chat_id, chunk, Some("HTML"), None);
                    }
                }
            }
        }
    }
}

fn handle_approval_request(
    token: &str,
    chat_id: i64,
    request_id: &str,
    action: &str,
    resource: &str,
    reason: &str,
    turns: &mut HashMap<i64, TurnState>,
    pending_approvals: &mut HashMap<String, PendingApproval>,
) {
    // Flush any in-progress text.
    if let Some(turn) = turns.get_mut(&chat_id) {
        if !turn.text_buffer.is_empty() && !turn.finalized {
            finalize_turn_text(token, chat_id, turn);
        }
    }

    pending_approvals.insert(
        request_id.to_string(),
        PendingApproval {
            chat_id,
        },
    );

    let escaped_action = format::html_escape(action);
    let escaped_resource = format::html_escape(resource);
    let escaped_reason = format::html_escape(reason);

    let text = format!(
        "Approval needed:\n\
         <b>Action:</b> {escaped_action}\n\
         <b>Resource:</b> <code>{escaped_resource}</code>\n\
         <b>Reason:</b> {escaped_reason}"
    );

    let keyboard = telegram::inline_keyboard(vec![
        ("Allow Once".into(), format!("apr:{request_id}:allow_once")),
        (
            "Allow Session".into(),
            format!("apr:{request_id}:allow_session"),
        ),
        ("Deny".into(), format!("apr:{request_id}:deny")),
    ]);

    let _ = telegram::send_message(token, chat_id, &text, Some("HTML"), Some(&keyboard));
}

fn handle_elicitation_request(token: &str, chat_id: i64, request_id: &str, field: Option<&Value>) {
    let prompt = field
        .and_then(|f| f.get("prompt"))
        .and_then(|p| p.as_str())
        .unwrap_or("Input required");

    // For enum-type fields (field_type is {"Enum": ["opt1", "opt2", ...]}),
    // show inline keyboard with options.
    if let Some(options) = field
        .and_then(|f| f.get("field_type"))
        .and_then(|t| t.get("Enum"))
        .and_then(|e| e.as_array())
    {
        let buttons: Vec<(String, String)> = options
            .iter()
            .filter_map(|o| o.as_str())
            .map(|o| (o.to_string(), format!("eli:{request_id}:{o}")))
            .collect();

        if !buttons.is_empty() {
            let keyboard = telegram::inline_keyboard(buttons);
            let _ = telegram::send_message(
                token,
                chat_id,
                &format::html_escape(prompt),
                Some("HTML"),
                Some(&keyboard),
            );
            return;
        }
    }

    // For text/secret fields, just show the prompt. The user replies with text.
    let _ = telegram::send_message(
        token,
        chat_id,
        &format!("Input required: {}", format::html_escape(prompt)),
        Some("HTML"),
        None,
    );
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn finalize_turn_text(token: &str, chat_id: i64, turn: &mut TurnState) {
    let html = format::md_to_telegram_html(&turn.text_buffer);
    let chunks = format::chunk_html(&html, 4000);

    if let Some((first, rest)) = chunks.split_first() {
        let _ = telegram::edit_message_text(token, chat_id, turn.msg_id, first, Some("HTML"));
        for chunk in rest {
            if let Ok(msg) = telegram::send_message(token, chat_id, chunk, Some("HTML"), None) {
                turn.msg_id = msg.message_id;
            }
        }
    }
    turn.finalized = true;
}

/// Find a chat that has an active turn (best-effort for events missing
/// session_id). Prefers the only active turn if there's exactly one.
fn find_chat_for_event(
    _session_to_chat: &HashMap<String, i64>,
    _sessions: &HashMap<i64, String>,
    turns: &HashMap<i64, TurnState>,
) -> Option<i64> {
    // If only one turn is active, it must be for the event.
    if turns.len() == 1 {
        return turns.keys().next().copied();
    }
    // Multiple turns active — can't disambiguate without session_id.
    // This is a limitation; the event should ideally carry a session_id.
    None
}

fn is_user_allowed(allowed: &[i64], user_id: Option<i64>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    user_id.is_some_and(|id| allowed.contains(&id))
}

fn parse_allowed_users(s: &str) -> Vec<i64> {
    s.split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

/// Generate a deterministic-ish session ID from chat_id + timestamp.
fn new_session_id(chat_id: i64) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("tg-{chat_id}-{ts:x}")
}

fn session_kv_key(chat_id: i64) -> String {
    format!("tg.session.{chat_id}")
}

fn save_session(chat_id: i64, session_id: &str) {
    let _ = kv::set_json(
        &session_kv_key(chat_id),
        &SessionEntry {
            session_id: session_id.to_string(),
        },
    );
}

fn delete_session(chat_id: i64) {
    let _ = kv::delete(&session_kv_key(chat_id));
}

fn load_sessions() -> HashMap<i64, String> {
    let keys = kv::list_keys("tg.session.").unwrap_or_default();
    let mut map = HashMap::new();
    for key in keys {
        if let Some(chat_id_str) = key.strip_prefix("tg.session.") {
            if let Ok(chat_id) = chat_id_str.parse::<i64>() {
                if let Ok(entry) = kv::get_json::<SessionEntry>(&key) {
                    map.insert(chat_id, entry.session_id);
                }
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allowed_users_basic() {
        assert_eq!(parse_allowed_users("123,456,789"), vec![123, 456, 789]);
    }

    #[test]
    fn parse_allowed_users_with_spaces() {
        assert_eq!(parse_allowed_users(" 123 , 456 , 789 "), vec![123, 456, 789]);
    }

    #[test]
    fn parse_allowed_users_empty_string() {
        assert_eq!(parse_allowed_users(""), Vec::<i64>::new());
    }

    #[test]
    fn parse_allowed_users_ignores_invalid() {
        assert_eq!(parse_allowed_users("123,abc,456"), vec![123, 456]);
    }

    #[test]
    fn parse_allowed_users_single() {
        assert_eq!(parse_allowed_users("42"), vec![42]);
    }

    #[test]
    fn is_user_allowed_empty_allows_all() {
        assert!(is_user_allowed(&[], Some(999)));
        assert!(is_user_allowed(&[], None));
    }

    #[test]
    fn is_user_allowed_present() {
        assert!(is_user_allowed(&[100, 200, 300], Some(200)));
    }

    #[test]
    fn is_user_allowed_absent() {
        assert!(!is_user_allowed(&[100, 200], Some(999)));
    }

    #[test]
    fn is_user_allowed_none_user_id() {
        assert!(!is_user_allowed(&[100], None));
    }

    #[test]
    fn new_session_id_format() {
        let sid = new_session_id(12345);
        assert!(sid.starts_with("tg-12345-"));
        // Should contain a hex timestamp after the second dash.
        let parts: Vec<&str> = sid.splitn(3, '-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "tg");
        assert_eq!(parts[1], "12345");
        assert!(u128::from_str_radix(parts[2], 16).is_ok());
    }

    #[test]
    fn new_session_id_unique() {
        let a = new_session_id(1);
        // Ensure a tiny sleep so the timestamp differs.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_session_id(1);
        assert_ne!(a, b);
    }
}
