//! Bounded admission and upstream reads, shared across configuration generations.
use crate::Error;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub(crate) struct Gate(AtomicUsize);
pub(crate) struct Permit(Arc<Gate>);
impl Gate {
    pub fn acquire(self: &Arc<Self>, limit: usize) -> Option<Permit> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < limit).then_some(n + 1)
            })
            .ok()?;
        Some(Permit(self.clone()))
    }
    pub fn used(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub bytes: usize,
    pub event: usize,
    pub idle: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 32 * 1024 * 1024,
            event: 2 * 1024 * 1024,
            idle: 60,
        }
    }
}
pub(crate) async fn json(response: reqwest::Response, max: usize) -> Result<Value, Error> {
    let mut stream = bytes(
        response,
        Limits {
            bytes: max,
            ..Limits::default()
        },
    );
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        data.extend_from_slice(&chunk?);
    }
    serde_json::from_slice(&data).map_err(|_| Error::InvalidResponse("invalid upstream JSON"))
}
pub(crate) async fn error_text(response: reqwest::Response) -> String {
    let mut stream = bytes(
        response,
        Limits {
            bytes: 4096,
            event: 4096,
            idle: 2,
        },
    );
    let mut data = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        data.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&data).into_owned()
}
pub(crate) fn bytes(
    response: reqwest::Response,
    limits: Limits,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
    Box::pin(futures_util::stream::unfold(
        (response.bytes_stream(), 0usize, false),
        move |(mut stream, mut total, ended)| async move {
            if ended {
                return None;
            }
            let next = tokio::time::timeout(Duration::from_secs(limits.idle), stream.next()).await;
            let result = match next {
                Ok(None) => return None,
                Ok(Some(Ok(chunk))) => {
                    total = total.saturating_add(chunk.len());
                    if total > limits.bytes {
                        Err(std::io::Error::other("upstream response size limit"))
                    } else {
                        Ok(chunk)
                    }
                }
                Ok(Some(Err(_))) => Err(std::io::Error::other("upstream transport failed")),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "upstream idle timeout",
                )),
            };
            let ended = result.is_err();
            Some((result, (stream, total, ended)))
        },
    ))
}
