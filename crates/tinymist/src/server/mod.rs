pub mod compile;
pub mod compile_cmd;
pub mod compile_init;
pub mod lsp;
pub mod lsp_cmd;
pub mod lsp_init;

use std::collections::HashMap;
use std::fmt::Display;
use std::future::ready;

use async_lsp::{ErrorCode, ResponseError};
use futures::future::BoxFuture;
use lsp_types::request::{ExecuteCommand, Request};
use serde::Deserialize;
use serde_json::{from_value, Value as JsonValue};

type ResponseResult<R> = Result<<R as Request>::Result, ResponseError>;
type ResponseFuture<R> = BoxFuture<'static, ResponseResult<R>>;

fn ok<R: Request>(res: R::Result) -> ResponseFuture<R> {
    Box::pin(ready(Ok(res)))
}

fn internal_error<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(internal_error_(msg))
}

fn internal_error_<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INTERNAL_ERROR, msg))
}

fn invalid_params<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(invalid_params_(msg))
}

fn invalid_params_<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INVALID_PARAMS, msg))
}

fn method_not_found<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(method_not_found_(msg))
}

fn method_not_found_<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::METHOD_NOT_FOUND, msg))
}

type ExecCmdHandler<S> = fn(&mut S, Vec<JsonValue>) -> ResponseFuture<ExecuteCommand>;
type ExecCmdMap<S> = HashMap<&'static str, ExecCmdHandler<S>>;

/// Get a parsed command argument.
/// Return `None` when no arg or parse failed.
fn parse_arg<'de, T: Deserialize<'de>>(args: &Vec<JsonValue>, idx: usize) -> Option<T> {
    args.get(idx).and_then(|x| from_value::<T>(x).ok())
}

/// Get a parsed command argument.
/// Return default when no arg.
/// Return `None` when parse failed.
fn parse_arg_or_default<'de, T: Deserialize<'de> + Default>(
    args: &Vec<JsonValue>,
    idx: usize,
) -> Option<T> {
    match args.get(idx) {
        Some(arg) => from_value(arg).ok(),
        None => Some(Default::default()),
    }
}
