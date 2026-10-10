//! The one address a test uses for "nothing listens here".
//!
//! A test that needs a connect to fail must not find its address by binding
//! `127.0.0.1:0`, reading the port and dropping the listener: the port goes
//! back to the ephemeral pool, and a test running beside it — every HTTP
//! responder in this workspace binds `127.0.0.1:0` — can be handed the same
//! port before the connect happens. The connect then succeeds against a
//! stranger's server.
//!
//! Port 1 has no such race. It sits below every operating system's ephemeral
//! range, so a `:0` bind is never handed it, and nothing in this workspace
//! binds it by number. A connect to it is refused by the kernel on Linux,
//! macOS and Windows (measured: immediate on Linux and macOS, about two
//! seconds on Windows, which is what Windows takes to report any refused
//! loopback connect).
//!
//! Holding a socket bound but not listening for the life of the test was
//! measured too and is not usable: Linux and Windows refuse a connect to it,
//! macOS lets the connect time out.
//!
//! The address is a constant, so a child process the test spawns gets the
//! same guarantee as the test itself: there is no guard to keep alive.
//! `.claude/rules/test-refusing-addr.md` holds the rule and its audit.

use std::net::{Ipv4Addr, SocketAddr};

/// A loopback address every connect to is refused, on every platform, for as
/// long as the test runs.
pub fn refusing_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_is_refused_and_does_not_time_out() {
        let err = std::net::TcpStream::connect_timeout(
            &refusing_addr(),
            std::time::Duration::from_secs(20),
        )
        .expect_err("nothing listens on port 1");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "{err}");
    }
}
