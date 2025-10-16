use compio::time::sleep;
use futures_util::{Stream, future::LocalBoxFuture, stream::FusedStream};
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

pin_project_lite::pin_project! {
    /// A stream adapter that debounces items from the underlying stream.
    ///
    /// This wrapper will only yield the last item from the wrapped stream after a
    /// specified period of inactivity. When a new item is received from the inner
    /// stream, a timer is started. If another item arrives before the timer
    /// expires, the timer is reset. The pending item is only yielded to the consumer
    /// when the timer successfully completes.
    ///
    /// This `struct` is created by the [`DebounceStream::debounce()`] method.
    #[must_use = "streams do nothing unless polled"]
    pub struct Debouncer<'a, S: Stream> {
        #[pin]
        stream: S,
        delay: Duration,
        pending_fut: Option<LocalBoxFuture<'a, S::Item>>,
    }
}

impl<S: Stream> Debouncer<'_, S> {
    /// Creates a new `Debouncer` that wraps the given stream.
    ///
    /// # Arguments
    ///
    /// * `stream`: The stream to debounce. It must implement `FusedStream`.
    /// * `delay`: The duration of inactivity to wait for before yielding the last item.
    pub fn new(stream: S, delay: Duration) -> Self {
        Self {
            stream,
            delay,
            pending_fut: None,
        }
    }
}

impl<'a, S: FusedStream> Stream for Debouncer<'a, S>
where
    S::Item: 'a,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // Eagerly drain the underlying stream of all immediately available items.
        while let Poll::Ready(Some(item)) = this.stream.as_mut().poll_next(cx) {
            let delay = *this.delay;
            // Each new item resets the debounce timer.
            // We create a new future that will complete after `delay` and yield the item.
            *this.pending_fut = Some(Box::pin(async move {
                sleep(delay).await;
                item
            }));
        }

        // Check if there is a pending item waiting for its delay to pass.
        if let Some(fut) = this.pending_fut.as_mut() {
            return fut.as_mut().poll(cx).map(|item| {
                *this.pending_fut = None;
                Some(item)
            });
        }

        // If the underlying stream is terminated and there's no pending future,
        if this.stream.is_terminated() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

impl<'a, S: FusedStream> FusedStream for Debouncer<'a, S>
where
    S::Item: 'a,
{
    fn is_terminated(&self) -> bool {
        // The debouncer is terminated only if the underlying stream is terminated
        // AND there is no pending item waiting to be yielded.
        self.stream.is_terminated() && self.pending_fut.is_none()
    }
}

pub trait DebounceStream: Stream {
    /// Debounces a stream with a given delay.
    ///
    /// This adapter will only yield the last item from the stream after a
    /// specified period of inactivity. For example, if you have a stream of user
    /// input events, you might use `debounce` to only process the input after the
    /// user has stopped typing for 500ms.
    ///
    /// Note: This convenience method requires the stream's items to have a lifetime
    /// of at least `'a`. For streams with items that cannot be boxed with a specific
    /// lifetime, you may need to use [`Debouncer::new()`] directly and manage lifetimes
    /// manually.
    ///
    /// # Example
    ///
    /// ```
    /// # use compio::time::{sleep, timeout};
    /// # use futures_util::{stream, StreamExt};
    /// # use std::time::Duration;
    /// # use compio_debouncer::DebounceStream; // In real code, this would be the crate path.
    /// #
    /// # #[compio::test]
    /// # async fn example() {
    /// let delay = Duration::from_millis(100);
    ///
    /// // Create a stream that yields three numbers in quick succession.
    /// let source_stream = stream::iter(vec![]).fuse();
    ///
    /// // Apply the debounce adapter.
    /// let mut debounced_stream = source_stream.debounce(delay);
    ///
    /// // Allow some time to pass, but less than the debounce delay.
    /// // No item should be ready yet.
    /// let res = timeout(Duration::from_millis(50), debounced_stream.next()).await;
    /// assert!(res.is_err());
    ///
    /// // Wait for the debounce delay to pass.
    /// sleep(Duration::from_millis(60)).await;
    ///
    /// // Now we should receive the *last* item that was sent, which is 3.
    /// assert_eq!(debounced_stream.next().await, Some(3));
    ///
    /// // The source stream is exhausted, so the debounced stream should also end.
    /// assert_eq!(debounced_stream.next().await, None);
    /// # }
    /// ```
    fn debounce<'a>(self, delay: Duration) -> Debouncer<'a, Self>
    where
        Self: Sized + FusedStream,
        Self::Item: 'a,
    {
        Debouncer::new(self, delay)
    }
}

impl<T: ?Sized> DebounceStream for T where T: Stream {}

#[cfg(test)]
mod tests {
    use super::*;
    use compio::time::{sleep, timeout};
    use futures_util::{StreamExt, stream};
    use std::time::Duration;

    #[compio::test]
    async fn test_stream_debounce_single_burst() {
        let delay = Duration::from_millis(100);
        let source_stream = stream::iter(vec![1, 2, 3]).fuse();

        let mut debounced_stream = Debouncer::new(source_stream, delay);

        let res = timeout(Duration::from_millis(50), debounced_stream.next()).await;
        assert!(res.is_err(), "Stream should not yield an item yet");

        sleep(Duration::from_millis(110)).await;

        assert_eq!(debounced_stream.next().await, Some(3));
        assert_eq!(debounced_stream.next().await, None);
    }

    #[compio::test]
    async fn test_stream_debounce_with_pauses() {
        let delay = Duration::from_millis(200);

        let stream_controller = stream::unfold(0, |state| async move {
            if state < 5 {
                let next_state = state + 1;
                let sleep_dur = if state == 2 || state == 4 {
                    Duration::from_millis(300)
                } else {
                    Duration::from_millis(50)
                };
                sleep(sleep_dur).await;
                Some((state, next_state))
            } else {
                None
            }
        })
        .fuse();

        let mut debounced_stream = stream_controller.debounce(delay).boxed_local();

        assert_eq!(
            timeout(Duration::from_millis(400), debounced_stream.next())
                .await
                .unwrap(),
            Some(1)
        );

        assert_eq!(
            timeout(Duration::from_millis(400), debounced_stream.next())
                .await
                .unwrap(),
            Some(3)
        );

        assert_eq!(
            timeout(Duration::from_millis(600), debounced_stream.next())
                .await
                .unwrap(),
            Some(4)
        );

        // This call used to cause the panic, but now it will work correctly.
        assert_eq!(debounced_stream.next().await, None);
    }

    #[compio::test]
    async fn test_stream_debounce_source_ends_before_delay() {
        let delay = Duration::from_millis(100);

        // .fuse() is correctly used here already
        let source_stream = stream::iter(vec![10, 20]).fuse();
        let mut debounced_stream = source_stream.debounce(delay);

        assert!(
            timeout(Duration::from_millis(50), debounced_stream.next())
                .await
                .is_err()
        );

        sleep(Duration::from_millis(60)).await;

        assert_eq!(debounced_stream.next().await, Some(20));
        assert_eq!(debounced_stream.next().await, None);
    }
}
