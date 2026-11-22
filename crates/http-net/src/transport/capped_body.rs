//! Reading an answer's body under the client's cap. A declared length past
//! the cap is refused before any of the body is read; a body whose bytes
//! pass it is cut there, the rest left unread. The buffer grows by doubling
//! and never past the cap, so no allocation holds more than the cap.

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Body;

/// Why a body was not read whole.
#[derive(Debug)]
pub(super) enum BodyError<E> {
    /// Its declared length passes the cap.
    Declared,
    /// The bytes read pass the cap.
    Read,
    /// Reading it failed.
    Failed(E),
}

/// The whole of `body`, at most `cap` bytes.
pub(super) async fn read<B>(body: &mut B, cap: usize) -> Result<Vec<u8>, BodyError<B::Error>>
where
    B: Body<Data = Bytes> + Unpin,
{
    let declared = body.size_hint().lower();
    if declared > cap as u64 {
        return Err(BodyError::Declared);
    }
    let mut out = Vec::with_capacity(usize::try_from(declared).unwrap_or(cap));
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.map_err(BodyError::Failed)?.into_data() else { continue };
        if data.len() > cap - out.len() {
            return Err(BodyError::Read);
        }
        grow(&mut out, data.len(), cap);
        out.extend_from_slice(&data);
    }
    Ok(out)
}

/// Room in `out` for `more` bytes, which fit under `cap`: twice the
/// capacity, or what is needed, at most `cap`.
fn grow(out: &mut Vec<u8>, more: usize, cap: usize) {
    let needed = out.len() + more;
    if needed <= out.capacity() {
        return;
    }
    let target = needed.max(out.capacity().saturating_mul(2)).min(cap);
    out.reserve_exact(target - out.len());
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use hyper::body::{Frame, SizeHint};

    use super::*;

    /// A body of chunks, declaring `declared` bytes when given.
    struct Chunks {
        chunks: VecDeque<Bytes>,
        declared: Option<u64>,
    }

    impl Chunks {
        fn of(sizes: &[usize], declared: Option<u64>) -> Self {
            Self { chunks: sizes.iter().map(|n| Bytes::from(vec![b'x'; *n])).collect(), declared }
        }
    }

    impl Body for Chunks {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            Poll::Ready(self.chunks.pop_front().map(|c| Ok(Frame::data(c))))
        }

        fn size_hint(&self) -> SizeHint {
            self.declared.map_or_else(SizeHint::new, SizeHint::with_exact)
        }
    }

    #[tokio::test]
    async fn a_body_at_the_cap_is_read_whole_and_past_it_refused_by_its_kind() {
        let whole = read(&mut Chunks::of(&[300, 300, 424], None), 1024).await.unwrap();
        assert_eq!(whole.len(), 1024);
        let declared = read(&mut Chunks::of(&[10], Some(1025)), 1024).await;
        assert!(matches!(declared, Err(BodyError::Declared)), "{declared:?}");
        let streamed = read(&mut Chunks::of(&[1000, 25], None), 1024).await;
        assert!(matches!(streamed, Err(BodyError::Read)), "{streamed:?}");
    }

    #[tokio::test]
    async fn the_buffer_doubles_and_never_holds_more_than_the_cap() {
        let body = read(&mut Chunks::of(&[100; 10], None), 1000).await.unwrap();
        assert_eq!((body.len(), body.capacity()), (1000, 1000), "100, 200, 400, 800, the cap");
        let body = read(&mut Chunks::of(&[100; 3], None), 1000).await.unwrap();
        assert_eq!((body.len(), body.capacity()), (300, 400));
        let declared = read(&mut Chunks::of(&[512, 512], Some(1024)), 1024).await.unwrap();
        assert_eq!(declared.capacity(), 1024, "the declared length, once");
    }
}
