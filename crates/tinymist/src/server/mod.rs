pub mod lsp;
pub mod lsp_init;

pub mod compiler;
pub mod compiler_init;

use std::fmt::Display;

use async_lsp::{ErrorCode, ResponseError};
use futures::future::BoxFuture;
use lsp_types::request::Request;

type ResponseResult<R> = Result<<R as Request>::Result, ResponseError>;
type ResponseFuture<R> = BoxFuture<'static, ResponseResult<R>>;

fn resp_fut_ok<R: Request>(res: R::Result) -> ResponseFuture<R> {
    Box::pin(async { Ok(res) })
}

fn resp_fut_err<R: Request>(msg: impl Display) -> ResponseFuture<R> {
    Box::pin(async { resp_err(msg) })
}

fn resp_err<R: Request>(msg: impl Display) -> ResponseResult<R> {
    Err(ResponseError::new(ErrorCode::INTERNAL_ERROR, msg))
}
