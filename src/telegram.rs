//! Telegram Bot API HTTP client.
//!
//! Calls the Telegram Bot API using the Astrid HTTP airlock. All methods are
//! synchronous (WASM single-threaded).

use astrid_sdk::prelude::*;
use serde_json::json;

use crate::types::{InlineKeyboardMarkup, TgMessage, TgResponse, Update};

const BASE_URL: &str = "https://api.telegram.org";

/// Parse a Telegram API response, mapping `ok: false` to an error.
fn parse_response<T: serde::de::DeserializeOwned>(
    resp: http::Response,
    method: &str,
) -> Result<T, SysError> {
    let parsed: TgResponse<T> = resp
        .json()
        .map_err(|e| SysError::ApiError(format!("{method}: failed to parse response: {e}")))?;

    if !parsed.ok {
        return Err(SysError::ApiError(format!(
            "{method}: {}",
            parsed.description.unwrap_or_else(|| "unknown error".into()),
        )));
    }

    parsed
        .result
        .ok_or_else(|| SysError::ApiError(format!("{method}: missing result")))
}

/// Poll for new updates from Telegram.
///
/// Uses long polling with the given timeout (seconds). A timeout of 0 returns
/// immediately (non-blocking poll).
pub fn get_updates(token: &str, offset: i64, timeout: u32) -> Result<Vec<Update>, SysError> {
    let url = format!(
        "{BASE_URL}/bot{token}/getUpdates?offset={offset}&timeout={timeout}&allowed_updates=[\"message\",\"callback_query\"]"
    );
    let req = http::Request::get(&url);
    let resp = http::send(&req)?;
    parse_response(resp, "getUpdates")
}

/// Send a text message to a chat.
pub fn send_message(
    token: &str,
    chat_id: i64,
    text: &str,
    parse_mode: Option<&str>,
    reply_markup: Option<&InlineKeyboardMarkup>,
) -> Result<TgMessage, SysError> {
    let mut body = json!({
        "chat_id": chat_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = json!(mode);
    }
    if let Some(markup) = reply_markup {
        body["reply_markup"] = serde_json::to_value(markup)
            .map_err(|e| SysError::ApiError(format!("Failed to serialize markup: {e}")))?;
    }

    let url = format!("{BASE_URL}/bot{token}/sendMessage");
    let req = http::Request::post(&url).json(&body)?;
    let resp = http::send(&req)?;
    parse_response(resp, "sendMessage")
}

/// Edit the text of an existing message.
pub fn edit_message_text(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    parse_mode: Option<&str>,
) -> Result<(), SysError> {
    let mut body = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = json!(mode);
    }

    let url = format!("{BASE_URL}/bot{token}/editMessageText");
    let req = http::Request::post(&url).json(&body)?;
    let resp = http::send(&req)?;

    // Telegram returns "message is not modified" when text is unchanged —
    // not a real error for our throttled-edit pattern.
    let body_text = resp.text().unwrap_or("");
    if body_text.contains("message is not modified") {
        return Ok(());
    }

    let parsed: TgResponse<serde_json::Value> = serde_json::from_str(body_text)
        .map_err(|e| SysError::ApiError(format!("editMessageText: parse error: {e}")))?;

    if !parsed.ok {
        return Err(SysError::ApiError(format!(
            "editMessageText: {}",
            parsed.description.unwrap_or_default(),
        )));
    }

    Ok(())
}

/// Answer a callback query (dismiss the "loading" spinner on inline buttons).
pub fn answer_callback_query(
    token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<(), SysError> {
    let mut body = json!({ "callback_query_id": callback_query_id });
    if let Some(t) = text {
        body["text"] = json!(t);
    }

    let url = format!("{BASE_URL}/bot{token}/answerCallbackQuery");
    let req = http::Request::post(&url).json(&body)?;
    let _ = http::send(&req)?;
    Ok(())
}

/// Send a "typing" chat action indicator.
pub fn send_typing(token: &str, chat_id: i64) -> Result<(), SysError> {
    let body = json!({
        "chat_id": chat_id,
        "action": "typing",
    });
    let url = format!("{BASE_URL}/bot{token}/sendChatAction");
    let req = http::Request::post(&url).json(&body)?;
    let _ = http::send(&req)?;
    Ok(())
}

/// Build an inline keyboard with a single row of buttons.
pub fn inline_keyboard(buttons: Vec<(String, String)>) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![
            buttons
                .into_iter()
                .map(|(text, data)| crate::types::InlineKeyboardButton {
                    text,
                    callback_data: data,
                })
                .collect(),
        ],
    }
}
