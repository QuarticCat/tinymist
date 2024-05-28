pub mod compile;
pub mod compile_cmd;
pub mod compile_init;
pub mod lsp;
pub mod lsp_cmd;
pub mod lsp_init;

use std::collections::HashMap;
use std::fmt::Display;
use std::future::ready;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use async_lsp::{ErrorCode, ResponseError};
use futures::future::BoxFuture;
use lsp_types::request::{ExecuteCommand, Request};
use serde_json::{from_value, Value as JsonValue};

pub enum TwoStage<Uninit, Inited> {
    Uninit(Uninit),
    Inited(Inited),
}

impl<Uninit, Inited> Deref for TwoStage<Uninit, Inited> {
    type Target = Inited;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Inited(this) => this,
            _ => panic!("uninitialized"),
        }
    }
}

impl<Uninit, Inited> DerefMut for TwoStage<Uninit, Inited> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Inited(this) => this,
            _ => panic!("uninitialized"),
        }
    }
}

fn try_<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    f()
}

fn try_or<T>(f: impl FnOnce() -> Option<T>, default: T) -> T {
    f().unwrap_or(default)
}

fn try_or_default<T: Default>(f: impl FnOnce() -> Option<T>) -> T {
    f().unwrap_or_default()
}

pub type ResponseResult<R> = Result<<R as Request>::Result, ResponseError>;
pub type ResponseFuture<R> = BoxFuture<'static, ResponseResult<R>>;

pub fn ok<R: Request>(res: R::Result) -> ResponseFuture<R> {
    Box::pin(ready(Ok(res)))
}

pub fn internal_error<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(ready(internal_error_::<R>(msg)))
}

pub fn internal_error_<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INTERNAL_ERROR, msg))
}

pub fn invalid_params<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(ready(invalid_params_::<R>(msg)))
}

pub fn invalid_params_<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INVALID_PARAMS, msg))
}

pub fn method_not_found<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(ready(method_not_found_::<R>(msg)))
}

pub fn method_not_found_<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::METHOD_NOT_FOUND, msg))
}

type ExecCmdHandler<S> = fn(&mut S, Vec<JsonValue>) -> ResponseFuture<ExecuteCommand>;
type ExecCmdMap<S> = HashMap<&'static str, ExecCmdHandler<S>>;
type ResourceMap<S> = HashMap<&'static Path, ExecCmdHandler<S>>;

/// Get a parsed command argument.
/// Return `INVALID_PARAMS` when no arg or parse failed.
macro_rules! get_arg {
    ($args:ident[$idx:expr] as $ty:ty) => {{
        let arg = $args.get_mut($idx);
        let arg = arg.and_then(|x| from_value::<$ty>(x.take()).ok());
        match arg {
            Some(v) => v,
            None => {
                return Box::pin(ready(Err(ResponseError::new(
                    ErrorCode::INVALID_PARAMS,
                    concat!("expect ", stringify!($ty), "at args[", $idx, "]"),
                ))));
            }
        }
    }};
}
use get_arg;

/// Get a parsed command argument or default if no arg.
/// Return `INVALID_PARAMS` when parse failed.
macro_rules! get_arg_or_default {
    ($args:ident[$idx:expr] as $ty:ty) => {{
        if $idx >= $args.len() {
            Default::default()
        } else {
            get_arg!($args[$idx] as $ty)
        }
    }};
}
use get_arg_or_default;
