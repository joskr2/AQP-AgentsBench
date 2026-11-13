use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::Result;

/// A boxed continuation: runs whatever is left of a middleware chain.
#[allow(type_alias_bounds)]
pub type Continuation<T: Send + 'static> =
    Arc<dyn Fn(T) -> BoxFuture<'static, Result<T>> + Send + Sync + 'static>;

/// One middleware in a [`Waterfall`].
///
/// A middleware receives the value and the rest of the chain. It may transform
/// the value, inspect the result, or short-circuit by returning without calling
/// [`Next::run`].
#[allow(type_alias_bounds)]
pub type Middleware<T: Send + 'static> =
    Arc<dyn Fn(T, Next<T>) -> BoxFuture<'static, Result<T>> + Send + Sync + 'static>;

/// The remainder of a middleware chain.
///
/// `Next` owns a boxed continuation rather than borrowing the chain, so the
/// future a middleware returns can be `'static`. This keeps the signatures
/// simple: no higher-ranked closure bounds and no lifetime plumbing.
pub struct Next<T: Send + 'static> {
    continuation: Continuation<T>,
}

impl<T: Send + 'static> Next<T> {
    /// Runs the remaining middlewares and returns the final value.
    pub async fn run(self, value: T) -> Result<T> {
        (self.continuation.as_ref())(value).await
    }
}

/// An ordered, composable middleware chain.
///
/// This is the minimal "waterfall" of DSH: each middleware wraps the next one,
/// so work happens in declaration order before the tail and in reverse order
/// after it. The continuation chain is rebuilt on each `run`; for the tiny
/// number of middlewares a harness needs this is irrelevant.
pub struct Waterfall<T: Send + 'static> {
    middleware: Vec<Middleware<T>>,
}

impl<T: Send + 'static> Waterfall<T> {
    pub fn new() -> Self {
        Self {
            middleware: Vec::new(),
        }
    }

    pub fn push<F>(&mut self, middleware: F)
    where
        F: Fn(T, Next<T>) -> BoxFuture<'static, Result<T>> + Send + Sync + 'static,
    {
        let middleware: Middleware<T> = Arc::new(middleware);
        self.middleware.push(middleware);
    }

    pub async fn run(&self, value: T) -> Result<T> {
        let mut continuation: Continuation<T> =
            Arc::new(|value: T| -> BoxFuture<'static, Result<T>> {
                Box::pin(async move { Ok(value) })
            });

        // Wrap the tail first so the earliest pushed middleware ends up
        // outermost, i.e. runs first.
        for middleware in self.middleware.iter().rev() {
            let middleware = Arc::clone(middleware);
            let previous = Arc::clone(&continuation);
            continuation = Arc::new(move |value: T| -> BoxFuture<'static, Result<T>> {
                let middleware = Arc::clone(&middleware);
                let previous = Arc::clone(&previous);
                let next = Next {
                    continuation: previous,
                };
                Box::pin(async move { (middleware.as_ref())(value, next).await })
            });
        }

        (continuation.as_ref())(value).await
    }

    pub fn len(&self) -> usize {
        self.middleware.len()
    }

    pub fn is_empty(&self) -> bool {
        self.middleware.is_empty()
    }
}

impl<T: Send + 'static> Default for Waterfall<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::HarnessEvent;

    #[tokio::test]
    async fn runs_in_order_and_rewrites_the_value() {
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let mut chain = Waterfall::<HarnessEvent>::new();

        let first = order.clone();
        chain.push(move |event, next| -> BoxFuture<'static, Result<HarnessEvent>> {
            let order = first.clone();
            Box::pin(async move {
                order.lock().unwrap().push("first-before");
                let event = next.run(event).await?;
                order.lock().unwrap().push("first-after");
                Ok(event)
            })
        });

        let second = order.clone();
        chain.push(move |event, next| -> BoxFuture<'static, Result<HarnessEvent>> {
            let order = second.clone();
            Box::pin(async move {
                order.lock().unwrap().push("second-before");
                let event = match event {
                    HarnessEvent::TextDelta { text } => HarnessEvent::TextDelta {
                        text: format!("{text}!"),
                    },
                    other => other,
                };
                let event = next.run(event).await?;
                order.lock().unwrap().push("second-after");
                Ok(event)
            })
        });

        let output = chain
            .run(HarnessEvent::TextDelta {
                text: "hi".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(
            output,
            HarnessEvent::TextDelta {
                text: "hi!".to_string()
            }
        );
        assert_eq!(
            *order.lock().unwrap(),
            vec!["first-before", "second-before", "second-after", "first-after"]
        );
    }

    #[tokio::test]
    async fn empty_waterfall_returns_the_value_untouched() {
        let chain = Waterfall::<HarnessEvent>::new();
        assert!(chain.is_empty());
        let output = chain.run(HarnessEvent::TurnStarted).await.unwrap();
        assert_eq!(output, HarnessEvent::TurnStarted);
    }
}
