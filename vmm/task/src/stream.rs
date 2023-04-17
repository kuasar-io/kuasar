/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{
    io::{Read, Result, Write},
    mem::{forget, MaybeUninit},
    os::unix::io::{AsRawFd, IntoRawFd, RawFd},
    pin::Pin,
    task::{Context, Poll},
};

use futures::ready;
use nix::{
    errno::Errno,
    unistd::{close, read, write},
};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

struct DirectFd(RawFd);

impl Read for &DirectFd {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        read(self.0, buf).map_err(Errno::into)
    }
}

impl Write for &DirectFd {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        write(self.0, buf).map_err(Errno::into)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl DirectFd {
    fn close(&mut self) -> Result<()> {
        close(self.0).map_err(Errno::into)
    }
}

impl Drop for DirectFd {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl AsRawFd for DirectFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

pub struct RawStream(AsyncFd<DirectFd>);

impl RawStream {
    pub fn new(fd: RawFd) -> Result<Self> {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
        Ok(Self(AsyncFd::new(DirectFd(fd))?))
    }
}

impl AsRawFd for RawStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for RawStream {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.as_raw_fd();
        forget(self);
        fd
    }
}

impl AsyncRead for RawStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        let b;
        unsafe {
            b = &mut *(buf.unfilled_mut() as *mut [MaybeUninit<u8>] as *mut [u8]);
        };

        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;

            match guard.try_io(|inner| inner.get_ref().read(b)) {
                Ok(Ok(n)) => {
                    unsafe {
                        buf.assume_init(n);
                    }
                    buf.advance(n);
                    return Ok(()).into();
                }
                Ok(Err(e)) => return Err(e).into(),
                Err(_would_block) => {
                    continue;
                }
            }
        }
    }
}

impl AsyncWrite for RawStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;

            match guard.try_io(|inner| inner.get_ref().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
}
