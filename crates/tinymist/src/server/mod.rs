pub mod lsp;
pub mod lsp_init;

pub mod compiler;
pub mod compiler_init;

use std::fmt::Display;
use std::future::ready;

use async_lsp::{ErrorCode, ResponseError};
use futures::future::BoxFuture;
use lsp_types::request::Request;

type ResponseResult<R> = Result<<R as Request>::Result, ResponseError>;
type ResponseFuture<R> = BoxFuture<'static, ResponseResult<R>>;

fn resp_fut_ok<R: Request>(res: R::Result) -> ResponseFuture<R> {
    Box::pin(ready(Ok(res)))
}

fn resp_fut_err<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(ready(internal_error(msg)))
}

fn internal_error<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INTERNAL_ERROR, msg))
}

fn invalid_params<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INVALID_PARAMS, msg))
}
