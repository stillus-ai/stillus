// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Cancellation polls wrap plain TCP reads before the standard TLS connector.
use crate::provider::Cancellation;
use std::{
    fmt,
    time::{Duration, Instant},
};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};

pub(super) struct CancelConnector(pub Cancellation);
impl fmt::Debug for CancelConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CancelConnector")
    }
}
impl<T: Transport> Connector<T> for CancelConnector {
    type Out = CancelTransport<T>;
    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| CancelTransport {
            inner,
            cancel: self.0.clone(),
        }))
    }
}
pub(super) struct CancelTransport<T> {
    inner: T,
    cancel: Cancellation,
}
impl<T> fmt::Debug for CancelTransport<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CancelTransport")
    }
}
impl<T: Transport> CancelTransport<T> {
    fn check(&self) -> Result<(), ureq::Error> {
        self.cancel.check().map_err(|_| {
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "request cancelled",
            ))
        })
    }
}
impl<T: Transport> Transport for CancelTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.check()?;
        // Never retry writes: the underlying transport may have sent a prefix.
        self.inner.transmit_output(amount, timeout)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let started = Instant::now();
        let budget = timeout.not_zero().map(|v| *v);
        loop {
            self.check()?;
            let remaining = budget.map(|b| b.saturating_sub(started.elapsed()));
            if remaining == Some(Duration::ZERO) {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let poll = Duration::from_millis(100);
            let next = NextTimeout {
                after: remaining.map(|r| r.min(poll)).unwrap_or(poll).into(),
                reason: timeout.reason,
            };
            match self.inner.await_input(next) {
                Err(ureq::Error::Timeout(_)) => continue,
                result => return result,
            }
        }
    }
    fn is_open(&mut self) -> bool {
        self.cancel.check().is_ok() && self.inner.is_open()
    }
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::unversioned::transport::LazyBuffers;
    #[derive(Debug)]
    struct Idle {
        buffers: LazyBuffers,
        polls: usize,
        stop: Option<Cancellation>,
    }
    impl Transport for Idle {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }
        fn transmit_output(&mut self, _: usize, _: NextTimeout) -> Result<(), ureq::Error> {
            panic!("read-only fixture")
        }
        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            self.polls += 1;
            assert!(*timeout.after <= Duration::from_millis(100));
            if self.polls == 3 {
                if let Some(cancel) = &self.stop {
                    cancel.cancel();
                } else {
                    return Ok(true);
                }
            }
            Err(ureq::Error::Timeout(timeout.reason))
        }
        fn is_open(&mut self) -> bool {
            true
        }
    }
    #[test]
    fn idle_polls_keep_one_connection_and_cancellation_stops_before_next_read() {
        for stop in [false, true] {
            let cancel = Cancellation::default();
            let inner = Idle {
                buffers: LazyBuffers::new(1024, 1024),
                polls: 0,
                stop: stop.then(|| cancel.clone()),
            };
            let mut transport = CancelTransport { inner, cancel };
            let result = transport.await_input(NextTimeout {
                after: Duration::from_secs(20).into(),
                reason: ureq::Timeout::Global,
            });
            assert_eq!(result.is_err(), stop);
            if stop {
                assert!(matches!(result, Err(ureq::Error::Io(ref error))
                    if error.kind() == std::io::ErrorKind::ConnectionAborted));
            }
            assert_eq!(transport.inner.polls, 3);
        }
    }
}
