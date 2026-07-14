// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

//! This module implements the Unix Domain Sockets backend for vsock - a mediator between
//! guest-side AF_VSOCK sockets and host-side AF_UNIX sockets. The heavy lifting is performed by
//! `muxer::VsockMuxer`, a connection multiplexer that uses `super::csm::VsockConnection` for
//! handling vsock connection states.
//!
//! Check out `muxer.rs` for a more detailed explanation of the inner workings of this backend.

#![allow(unused)]
mod muxer;
mod muxer_killq;
mod muxer_rxq;
use std::os::unix::net::UnixStream;
use std::fs::File;
use std::io::{Error as IoError, Read, Write};
use log::error;
use nix::errno::Errno;
use sendfd::RecvWithFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

pub use muxer::VsockMuxer as VsockUnixBackend;
use thiserror::Error;

mod defs {
    /// Maximum number of established connections that we can handle.
    pub const MAX_CONNECTIONS: usize = 1023;

    /// Size of the muxer RX packet queue.
    pub const MUXER_RXQ_SIZE: usize = 256;

    /// Size of the muxer connection kill queue.
    pub const MUXER_KILLQ_SIZE: usize = 128;
}

#[derive(Error, Debug)]
pub enum Error {
    /// Error converting from UTF-8
    #[error("Error converting from UTF-8")]
    ConvertFromUtf8(#[source] std::str::Utf8Error),
    /// Error registering a new epoll-listening FD.
    #[error("Error registering a new epoll-listening FD")]
    EpollAdd(#[source] std::io::Error),
    /// Error creating an epoll FD.
    #[error("Error creating an epoll FD")]
    EpollFdCreate(#[source] std::io::Error),
    /// The host made an invalid vsock port connection request.
    #[error("The host made an invalid vsock port connection request")]
    InvalidPortRequest,
    /// Error parsing integer.
    #[error("Error parsing integer")]
    ParseInteger(#[source] std::num::ParseIntError),
    /// Error reading stream port.
    #[error("Error reading stream port")]
    ReadStreamPort(#[source] Box<Error>),
    /// Error accepting a new connection from the host-side Unix socket.
    #[error("Error accepting a new connection from the host-side Unix socket")]
    UnixAccept(#[source] std::io::Error),
    /// Error binding to the host-side Unix socket.
    #[error("Error binding to the host-side Unix socket")]
    UnixBind(#[source] std::io::Error),
    /// Error connecting to a host-side Unix socket.
    #[error("Error connecting to a host-side Unix socket")]
    UnixConnect(#[source] std::io::Error),
    /// Error reading from host-side Unix socket.
    #[error("Error reading from host-side Unix socket")]
    UnixRead(#[source] std::io::Error),
    /// Muxer connection limit reached.
    #[error("Muxer connection limit reached")]
    TooManyConnections,
}
type Result<T> = std::result::Result<T, Error>;

pub struct HybridStream {
    pub hybrid_stream: File,
    pub slave_stream: Option<UnixStream>,
}

enum StreamVsock {
    UnixStream(UnixStream),
    HybridStream(HybridStream),
}

type MuxerConnection = super::csm::VsockConnection<StreamVsock>;

impl AsRawFd for StreamVsock {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            StreamVsock::UnixStream(s) => s.as_raw_fd(),
            StreamVsock::HybridStream(h) => h.hybrid_stream.as_raw_fd(),
        }
    }
}

impl Read for StreamVsock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            StreamVsock::UnixStream(s) => s.read(buf),
            StreamVsock::HybridStream(h) => h.hybrid_stream.read(buf),
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        match self {
            StreamVsock::UnixStream(s) => s.read_exact(buf),
            StreamVsock::HybridStream(h) => h.hybrid_stream.read_exact(buf),
        }
    }
}

impl Write for StreamVsock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            StreamVsock::UnixStream(s) => s.write(buf),
            StreamVsock::HybridStream(h) => {
                if let Some(mut stream) = h.slave_stream.take() {
                    stream.write(buf)
                } else {
                    h.hybrid_stream.write(buf)
                }
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            StreamVsock::UnixStream(s) => s.flush(),
            StreamVsock::HybridStream(h) => h.hybrid_stream.flush(),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            StreamVsock::UnixStream(s) => s.write_all(buf),
            StreamVsock::HybridStream(h) => {
                if let Some(mut stream) = h.slave_stream.take() {
                    stream.write_all(buf)
                } else {
                    h.hybrid_stream.write_all(buf)
                }
            }
        }
    }
}

impl StreamVsock {
    fn set_nonblocking(&mut self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            StreamVsock::UnixStream(s) => s.set_nonblocking(nonblocking),
            StreamVsock::HybridStream(h) => {
                let fd = h.hybrid_stream.as_raw_fd();
                let mut flag = unsafe { libc::fcntl(fd, libc::F_GETFL) };

                if nonblocking {
                    flag = flag | libc::O_NONBLOCK;
                } else {
                    flag = flag & !libc::O_NONBLOCK;
                }

                let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flag) };

                if ret < 0 {
                    error!("failed to set fcntl for fd {} with ret {}", fd, ret);
                    return Err(IoError::last_os_error());
                }

                Ok(())
            }
        }
    }

    fn set_read_timeout(&mut self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            StreamVsock::UnixStream(s) => UnixStream::set_read_timeout(s, dur),
            StreamVsock::HybridStream(_h) => {
                error!("unsupported!");
                Err(Errno::ENOPROTOOPT.into())
            }
        }
    }

    fn set_write_timeout(&mut self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            StreamVsock::UnixStream(s) => UnixStream::set_write_timeout(s, dur),
            StreamVsock::HybridStream(_h) => {
                error!("unsupported!");
                Err(Errno::ENOPROTOOPT.into())
            }
        }
    }

    fn recv_data_fd(
        &self,
        bytes: &mut [u8],
        fds: &mut [RawFd],
    ) -> std::io::Result<(usize, usize)> {
        match self {
            StreamVsock::UnixStream(s) => s.recv_with_fd(bytes, fds),
            StreamVsock::HybridStream(_h) => Err(Errno::ENOPROTOOPT.into()),
        }
    }
}
