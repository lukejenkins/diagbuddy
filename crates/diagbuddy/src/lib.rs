// SPDX-License-Identifier: Apache-2.0
//! diagbuddy: a DCI session on a DIAG stack, served to local stream clients as raw-HDLC DIAG
//! frames, each client with its own log mask. `main.rs` is a thin command line over these modules.

pub mod daemon;
pub mod diagcmd;
pub mod profile;
pub mod stream;
