// SPDX-License-Identifier: Apache-2.0
//! A DCI client for the DIAG stack, std only.
//!
//! * [`transport`]: reach the stack over `/dev/diag` or the userspace diag-router socket.
//! * [`dci`]: request numbers, the subscription message and the data-read parser.
//! * [`frame`]: re-frame a log record as HDLC DIAG.
//! * [`sys`]: the handful of libc calls, declared by hand.

pub mod dci;
pub mod frame;
pub mod sys;
pub mod transport;
