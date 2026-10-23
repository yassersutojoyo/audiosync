//! The errors the API answers with: `{error, message}`, where `error` is a
//! stable code the page acts on and `message` the English text it may show.
//! The page's own strings live in the page; these are the server's.

use serde_json::json;

/// One error answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: &str) -> ApiError {
        ApiError {
            status,
            code,
            message: message.to_owned(),
        }
    }

    /// The JSON body.
    pub fn body(&self) -> String {
        json!({ "error": self.code, "message": self.message }).to_string()
    }
}

pub fn bad_json() -> ApiError {
    ApiError::new(
        400,
        "bad_json",
        "The request isn't valid JSON for this action.",
    )
}

pub fn nothing_to_change() -> ApiError {
    ApiError::new(400, "nothing_to_change", "The request changes nothing.")
}

pub fn bad_request() -> ApiError {
    ApiError::new(400, "bad_request", "The request couldn't be read.")
}

pub fn not_paired() -> ApiError {
    ApiError::new(401, "not_paired", "This phone isn't paired with AudioSync.")
}

pub fn wrong_code() -> ApiError {
    ApiError::new(
        401,
        "wrong_code",
        "That code isn't right. Check the code in AudioSync.",
    )
}

pub fn code_expired() -> ApiError {
    ApiError::new(
        401,
        "code_expired",
        "That code has expired. Select New Code in AudioSync's Web Remote window.",
    )
}

pub fn bad_host() -> ApiError {
    ApiError::new(403, "bad_host", "Open the address AudioSync shows.")
}

pub fn bad_origin() -> ApiError {
    ApiError::new(
        403,
        "bad_origin",
        "Requests must come from the AudioSync page.",
    )
}

pub fn not_found() -> ApiError {
    ApiError::new(404, "not_found", "There's nothing at this address.")
}

pub fn unknown_speaker() -> ApiError {
    ApiError::new(
        404,
        "unknown_speaker",
        "AudioSync doesn't have this speaker anymore.",
    )
}

pub fn unknown_zone() -> ApiError {
    ApiError::new(
        404,
        "unknown_zone",
        "AudioSync doesn't have this zone anymore.",
    )
}

pub fn unknown_preset() -> ApiError {
    ApiError::new(
        404,
        "unknown_preset",
        "AudioSync doesn't have this preset anymore.",
    )
}

pub fn method_not_allowed() -> ApiError {
    ApiError::new(
        405,
        "method_not_allowed",
        "This address doesn't take that method.",
    )
}

pub fn speaker_unavailable() -> ApiError {
    ApiError::new(
        409,
        "speaker_unavailable",
        "This speaker isn't available right now.",
    )
}

pub fn zone_unavailable() -> ApiError {
    ApiError::new(
        409,
        "zone_unavailable",
        "None of this zone's speakers is available right now.",
    )
}

pub fn calibrating() -> ApiError {
    ApiError::new(
        409,
        "calibrating",
        "AudioSync is calibrating. Try again when it's done.",
    )
}

pub fn too_large() -> ApiError {
    ApiError::new(413, "too_large", "The request is too large.")
}

pub fn volume_out_of_range() -> ApiError {
    ApiError::new(422, "out_of_range", "Volume must be from 0 to 100.")
}

pub fn delay_out_of_range(min_ms: i64, max_ms: i64) -> ApiError {
    ApiError {
        status: 422,
        code: "out_of_range",
        message: format!("Delay must be from {min_ms} to {max_ms} ms."),
    }
}

pub fn too_many_tries() -> ApiError {
    ApiError::new(
        429,
        "too_many_tries",
        "Too many wrong codes. Wait a minute and try again.",
    )
}

pub fn pairing_locked() -> ApiError {
    ApiError::new(
        429,
        "pairing_locked",
        "Too many wrong codes. Select New Code in AudioSync's Web Remote window to pair again.",
    )
}

pub fn not_ready() -> ApiError {
    ApiError::new(503, "not_ready", "Connecting to AudioSync.")
}

pub fn busy() -> ApiError {
    ApiError::new(
        503,
        "busy",
        "AudioSync is serving too many connections. Try again.",
    )
}

/// The reason phrase of a status this server sends (RFC 9110 §15).
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "",
    }
}
