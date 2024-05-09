//! Language Server lifecycle.
//!
//! *Only applies to Language Servers.*
//!
//! This middleware handles
//! [the lifecycle of Language Servers](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#lifeCycleMessages),
//! specifically:
//! - Exit the main loop with `ControlFlow::Break(Ok(()))` on `exit`
//!   notification.
//! - Responds unrelated requests with errors and ignore unrelated notifications
//!   during initialization and shutting down.
use std::future::{ready, Future, Ready};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::Either;
use lsp_types::notification::{self, Notification};
use lsp_types::request::{self, Request};
use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

use async_lsp::{
    AnyEvent, AnyNotification, AnyRequest, Error, ErrorCode, LspService, ResponseError, Result,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum StateEnum {
    Uninitialized,
    Initializing,
    Ready,
    ShuttingDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State<Args, S> {
    Uninitialized(Option<Box<Args>>),
    Initializing(S),
    Ready(S),
    ShuttingDown,
}

impl<Args, S> State<Args, S> {
    fn enum_(&self) -> StateEnum {
        match self {
            Self::Uninitialized(..) => StateEnum::Uninitialized,
            Self::Initializing(..) => StateEnum::Initializing,
            Self::Ready(..) => StateEnum::Ready,
            Self::ShuttingDown => StateEnum::ShuttingDown,
        }
    }

    fn service(&mut self) -> Option<&mut S> {
        match self {
            Self::Initializing(s) | Self::Ready(s) => Some(s),
            _ => None,
        }
    }

    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>
    where
        S: LspService,
    {
        self.service()
            .map_or(ControlFlow::Continue(()), |s| s.notify(notif))
    }

    fn ack_initialized(&mut self) -> Result<()> {
        let mut s = Self::ShuttingDown;
        std::mem::swap(self, &mut s);
        match s {
            Self::Initializing(s) => {
                *self = Self::Ready(s);
                Ok(())
            }
            _ => {
                std::mem::swap(self, &mut s);
                Err(Error::Protocol(format!(
                    "Unexpected initialized notification on state {:?}",
                    self.enum_()
                )))
            }
        }
    }
}

impl<Args: Default, S> Default for State<Args, S> {
    fn default() -> Self {
        Self::Uninitialized(Default::default())
    }
}

pub trait Initializer {
    type S: LspService;

    fn initialize(self, req: AnyRequest) -> (Self::S, <Self::S as Service<AnyRequest>>::Future);
}

impl<T> Initializer for T
where
    T: LspService,
{
    type S = T;

    fn initialize(
        mut self,
        req: AnyRequest,
    ) -> (Self::S, <Self::S as Service<AnyRequest>>::Future) {
        let res = self.call(req);
        (self, res)
    }
}

/// The middleware handling Language Server lifecycle.
///
/// See [module level documentations](self) for details.
#[derive(Debug)]
pub struct Lifecycle<Args, S> {
    state: State<Args, S>,
}

impl<Args, S: LspService> Lifecycle<Args, S> {
    /// Creating the `Lifecycle` middleware in uninitialized state.
    #[must_use]
    pub fn new(just: Args) -> Self
    where
        Args: LspService,
    {
        Self {
            state: State::Uninitialized(Some(Box::new(just))),
        }
    }

    /// Creating the `Lifecycle` middleware in uninitialized state.
    #[must_use]
    pub fn new_staged(args: Args) -> Self
    where
        Args: Initializer<S = S>,
    {
        Self {
            state: State::Uninitialized(Some(Box::new(args))),
        }
    }
}

impl<Args, S> Service<AnyRequest> for Lifecycle<Args, S>
where
    Args: Initializer<S = S>,
    S: LspService,
    S::Error: From<ResponseError>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.state
            .service()
            .map_or(Poll::Ready(Ok(())), |s| s.poll_ready(cx))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let inner = match (&mut self.state, &*req.method) {
            (State::Uninitialized(args), request::Initialize::METHOD) => {
                let (s, res) = args.take().unwrap().initialize(req);
                self.state = State::Initializing(s);
                Either::Left(res)
            }
            (State::Uninitialized(..) | State::Initializing(..), _) => {
                Either::Right(ready(Err(ResponseError::new(
                    ErrorCode::SERVER_NOT_INITIALIZED,
                    "Server is not initialized yet",
                )
                .into())))
            }
            (_, request::Initialize::METHOD) => Either::Right(ready(Err(ResponseError::new(
                ErrorCode::INVALID_REQUEST,
                "Server is already initialized",
            )
            .into()))),
            (State::Ready(s), _) => {
                let is_shutdown = req.method == request::Shutdown::METHOD;
                let res = s.call(req);
                if is_shutdown {
                    self.state = State::ShuttingDown;
                }
                Either::Left(res)
            }
            (State::ShuttingDown, _) => Either::Right(ready(Err(ResponseError::new(
                ErrorCode::INVALID_REQUEST,
                "Server is shutting down",
            )
            .into()))),
        };
        ResponseFuture { inner }
    }
}

pin_project! {
    /// The [`Future`] type used by the [`Lifecycle`] middleware.
    pub struct ResponseFuture<Fut: Future> {
        #[pin]
        inner: Either<Fut, Ready<Fut::Output>>,
    }
}

impl<Fut: Future> Future for ResponseFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

impl<Args, S: LspService> LspService for Lifecycle<Args, S>
where
    Args: Initializer<S = S>,
    S::Error: From<ResponseError>,
{
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        match &*notif.method {
            notification::Exit::METHOD => {
                self.state.notify(notif)?;
                ControlFlow::Break(Ok(()))
            }
            notification::Initialized::METHOD => {
                if let Err(err) = self.state.ack_initialized() {
                    return ControlFlow::Break(Err(err));
                };
                self.state.notify(notif)?;
                ControlFlow::Continue(())
            }
            // todo: whether it is safe to ignore notifications
            _ => self.state.notify(notif),
        }
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        self.state
            .service()
            .map_or(ControlFlow::Continue(()), |s| s.emit(event))
    }
}

/// A [`tower_layer::Layer`] which builds [`Lifecycle`].
#[must_use]
#[derive(Clone, Default)]
pub struct LifecycleLayer {
    _private: (),
}

impl<S: LspService> Layer<S> for LifecycleLayer {
    type Service = Lifecycle<S, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Lifecycle::new(inner)
    }
}

/// A [`tower_layer::Layer`] which builds [`Lifecycle`] with staged
/// initialization.
#[must_use]
#[derive(Clone, Default)]
pub struct StagedLifecycleLayer<Args> {
    _private: std::marker::PhantomData<fn(Args)>,
}

impl<Args: Initializer> Layer<Args> for StagedLifecycleLayer<Args> {
    type Service = Lifecycle<Args, Args::S>;

    fn layer(&self, inner: Args) -> Self::Service {
        Lifecycle::new_staged(inner)
    }
}
