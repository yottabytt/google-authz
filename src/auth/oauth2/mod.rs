use std::{
    convert::TryFrom as _,
    fmt,
    sync::{Arc, RwLock},
    task::{self, Poll},
    time::Instant,
};

use hyper::{
    header::{self, AUTHORIZATION},
    Request,
};
use tracing::{info, instrument};

use crate::{auth, sync::RefGuard};

mod http;
pub(super) mod token;

mod metadata;
mod service_account;
mod user;

pub use metadata::Metadata;
pub use service_account::ServiceAccount;
pub use user::User;

#[derive(Clone)]
pub(super) struct Oauth2 {
    inner: Arc<RwLock<Inner>>,
}

impl Oauth2 {
    pub fn new(fetcher: Box<dyn token::Fetcher>, max_retry: u8) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner { state: State::NotFetched, fetcher, max_retry })),
        }
    }

    pub fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<auth::Result<()>> {
        if self.inner.try_read().unwrap().can_skip_poll_ready() {
            info!("can skip poll ready");
            return Poll::Ready(Ok(()));
        } 
        //info!("cannot skip poll ready");
        self.inner.try_write().unwrap().poll_ready(cx)
    }

    #[inline]
    pub fn add_header<B>(&self, mut req: Request<B>) -> Request<B> {
        req.headers_mut().insert(AUTHORIZATION, self.inner.try_read().unwrap().value());
        req
    }
}

impl fmt::Debug for Oauth2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Oauth2").field("inner", &self.inner).finish()
    }
}

struct Inner {
    state: State,
    fetcher: Box<dyn token::Fetcher>,
    max_retry: u8,
}

impl Inner {
    #[inline]
    fn can_skip_poll_ready(&self) -> bool {
        matches!(self.state, State::Fetched { ref current } if !current.expired(Instant::now()))
    }

    #[inline]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<auth::Result<()>> {
        macro_rules! poll {
            ($variant:ident, $future:expr, $attempts:ident) => {
                poll!($variant, $future, $attempts,)
            };
            ($variant:ident, $future:expr, $attempts:ident, $($field:ident),*) => {
                match $future.get_mut().as_mut().poll(cx) {
                    Poll::Ready(resp) => match resp.and_then(token::Token::try_from) {
                        Ok(token) => {
                            let expiry = token.expiry;
                            self.state = State::Fetched { current: token };
                            info!("fetched token: expiry={:?} state={:?}", expiry, self.state);
                            break Poll::Ready(Ok(()));
                        }
                        Err(err) => {
                            if $attempts > self.max_retry {
                                info!("max retries passed");
                                break Poll::Ready(Err(err));
                            }
                            info!("an error occurred during token fetching: attempts={}, err={:?}", $attempts, err);
                            self.state = State::$variant {
                                future: RefGuard::new(self.fetcher.fetch()),
                                attempts: $attempts + 1,
                                $(
                                    $field: $field.clone(),
                                )*
                            };
                            break Poll::Pending;
                        }
                    },
                    Poll::Pending => break Poll::Pending,
                }
            };
        }

        loop {
            info!("before matching on states");
            match self.state {
                State::NotFetched => {
                    info!("token is not fetched");
                    self.state = State::Fetching {
                        future: RefGuard::new(self.fetcher.fetch()),
                        attempts: 1,
                    };
                    info!("changing state to {:?}", self.state);
                    //continue;
                }
                State::Fetching { ref mut future, attempts } => {
                    info!("about to fetch token");
                    poll!(Fetching, future, attempts)
                }
                State::Refetching { ref mut future, attempts, ref last } => {
                    info!("about to refetch token");
                    poll!(Refetching, future, attempts, last)
                }
                State::Fetched { ref current } => {
                    if !current.expired(Instant::now()) {
                        info!("token is not expired yet and will expire: expiry={:?}", current.expiry);
                        break Poll::Ready(Ok(()));
                    }
                    info!("token expired and refetching");
                    self.state = State::Refetching {
                        future: RefGuard::new(self.fetcher.fetch()),
                        attempts: 1,
                        last: current.clone(),
                    };
                    //continue;
                }
            }
        }
    }

    #[inline]
    fn value(&self) -> header::HeaderValue {
        match self.state {
            State::Fetched { ref current } => current.value.clone(),
            State::Refetching { ref last, .. } => last.value.clone(),
            _ => unreachable!("invalid state: {:?}", self.state),
        }
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("state", &self.state)
            .field("fetcher", &self.fetcher)
            .field("max_retry", &self.max_retry)
            .finish()
    }
}

enum State {
    NotFetched,
    Fetching { future: RefGuard<token::ResponseFuture>, attempts: u8 },
    Refetching { future: RefGuard<token::ResponseFuture>, attempts: u8, last: token::Token },
    Fetched { current: token::Token },
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFetched => write!(f, "NotFetched"),
            Self::Fetching { .. } => write!(f, "Fetching"),
            Self::Refetching { .. } => write!(f, "Refetching"),
            Self::Fetched { .. } => write!(f, "Fetched"),
        }
    }
}
