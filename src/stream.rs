use std::{io, mem};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::Stream;
use pin_project::pin_project;
use tokio::io::ReadBuf;

use crate::RangeBody;

const IO_BUFFER_SIZE: usize = 64 * 1024;

#[pin_project]
pub struct RangedStream<B> {
    state: StreamState,
    #[pin]
    body: B,
}

impl<B: RangeBody> RangedStream<B> {
    pub fn new(body: B, start: u64, length: u64) -> Self {
        RangedStream {
            state: StreamState::Seek { start, remaining: length },
            body,
        }
    }
}

enum StreamState {
    Seek { start: u64, remaining: u64 },
    Seeking { remaining: u64 },
    Reading { buffer: BytesMut, remaining: u64 },
}

impl<B: RangeBody> Stream for RangedStream<B> {
    type Item = io::Result<Bytes>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<Option<io::Result<Bytes>>> {
        let mut this = self.project();

        if let StreamState::Seek { start, remaining } = *this.state {
            match this.body.as_mut().start_seek(start) {
                Err(e) => { return Poll::Ready(Some(Err(e))); }
                Ok(()) => { *this.state = StreamState::Seeking { remaining }; }
            }
        }

        if let StreamState::Seeking { remaining } = *this.state {
            match this.body.as_mut().poll_complete(cx) {
                Poll::Pending => { return Poll::Pending; }
                Poll::Ready(Err(e)) => { return Poll::Ready(Some(Err(e))); }
                Poll::Ready(Ok(())) => {
                    let buffer = allocate_buffer();
                    *this.state = StreamState::Reading { buffer, remaining };
                }
            }
        }

        if let StreamState::Reading { buffer, remaining } = this.state {
            let uninit = buffer.spare_capacity_mut();

            // calculate max number of bytes to read in this iteration, the
            // smaller of the buffer size and the number of bytes remaining
            let nbytes = std::cmp::min(
                uninit.len(),
                usize::try_from(*remaining).unwrap_or(usize::MAX),
            );

            let mut read_buf = ReadBuf::uninit(&mut uninit[0..nbytes]);

            match this.body.as_mut().poll_read(cx, &mut read_buf) {
                Poll::Pending => { return Poll::Pending; }
                Poll::Ready(Err(e)) => { return Poll::Ready(Some(Err(e))); }
                Poll::Ready(Ok(())) => {
                    match read_buf.filled().len() {
                        0 => { return Poll::Ready(None); }
                        n => {
                            // SAFETY: poll_read has filled the buffer with `n`
                            // additional bytes. `buffer.len` should always be
                            // 0 here, but include it for rigorous correctness
                            unsafe { buffer.set_len(buffer.len() + n); }

                            // replace state buffer and take this one to return
                            let chunk = mem::replace(buffer, allocate_buffer());

                            // subtract the number of bytes we just read from
                            // state.remaining, this usize->u64 conversion is
                            // guaranteed to always succeed, because n cannot be
                            // larger than remaining due to the cmp::min above
                            *remaining -= u64::try_from(n).unwrap();

                            // return this chunk
                            return Poll::Ready(Some(Ok(chunk.freeze())));
                        }
                    }
                }
            }
        }

        unreachable!();
    }
}

fn allocate_buffer() -> BytesMut {
    BytesMut::with_capacity(IO_BUFFER_SIZE)
}