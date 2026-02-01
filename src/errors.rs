use napi::{Env, Error, Result, Status};

pub mod codes {
    pub const INVALID_PROXY_OPTIONS: &str = "InvalidProxyOptions";
    pub const INVALID_APPLICATION_OPTIONS: &str = "InvalidApplicationOptions";
    pub const UNKNOWN_APPLICATION: &str = "UnknownApplication";
    pub const ALREADY_STARTED: &str = "AlreadyStarted";
    pub const LISTEN_BIND_FAILED: &str = "ListenBindFailed";
    pub const UNSUPPORTED_UPSTREAM_TYPE: &str = "UnsupportedUpstreamType";
    pub const UPSTREAM_ALREADY_EXISTS: &str = "UpstreamAlreadyExists";
    pub const UPSTREAM_NOT_FOUND: &str = "UpstreamNotFound";
}

pub fn type_error(env: &Env, code: &str, msg: impl AsRef<str>) -> Error {
    let msg = msg.as_ref();
    let _ = env.throw_type_error(msg, Some(code));
    Error::new(Status::InvalidArg, msg.to_owned())
}

pub fn generic_error(env: &Env, code: &str, msg: impl AsRef<str>) -> Error {
    let msg = msg.as_ref();
    let _ = env.throw_error(msg, Some(code));
    Error::new(Status::GenericFailure, msg.to_owned())
}

pub fn throw_type_error<T>(env: &Env, code: &str, msg: impl AsRef<str>) -> Result<T> {
    Err(type_error(env, code, msg))
}
