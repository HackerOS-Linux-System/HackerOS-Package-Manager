use serde::Serialize;
use std::process::exit;

#[derive(Serialize)]
pub struct ErrorPayload {
    pub err: ErrorInner,
}

#[derive(Serialize)]
pub struct ErrorInner {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ErrorCode {
    Success = 0,
    InvalidArgs = 1,
    PackageNotFound = 2,
    DependencyCycle = 3,
    InstallFailed = 4,
    RemoveFailed = 5,
    VerificationFailed = 6,
    UnknownCommand = 99,
}

pub fn output_error(code: ErrorCode, msg: &str) {
    let payload = ErrorPayload {
        err: ErrorInner {
            code: code as i32,
            message: msg.to_string(),
        },
    };
    let json = serde_json::to_string(&payload).expect("JSON marshal failed");
    eprintln!("{}", json);
    exit(code as i32);
}
