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

macro_rules! resp {
    ($expr:expr) => {
        Box::pin(ready($expr))
    };
}
use resp;

pub fn internal_error(msg: impl Display) -> ResponseError {
    ResponseError::new(ErrorCode::INTERNAL_ERROR, msg)
}

pub fn invalid_params(msg: impl Display) -> ResponseError {
    ResponseError::new(ErrorCode::INVALID_PARAMS, msg)
}

pub fn method_not_found(msg: impl Display) -> ResponseError {
    ResponseError::new(ErrorCode::METHOD_NOT_FOUND, msg)
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
                let msg = concat!("expect ", stringify!($ty), "at args[", $idx, "]");
                return Box::pin(ready(Err(invalid_params(msg))));
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
