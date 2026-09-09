// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::io::{self, Write};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Stage {
    WindowClosed,
    WindowSettingsFlushed,
    WindowSettingsFailed,
    EventLoopExited,
    FinalSettingsFlushed,
    FinalSettingsFailed,
    ShutdownComplete,
}

pub(crate) fn emit(stage: Stage) {
    let enabled = std::env::var("STILLUS_NATIVE_DIAGNOSTICS").as_deref() == Ok("1");
    if enabled {
        // Diagnostics must not turn a closed output pipe into a shutdown panic.
        let _ = write_event(enabled, stage, &mut io::stderr().lock());
    }
}

fn write_event(enabled: bool, stage: Stage, output: &mut impl Write) -> io::Result<()> {
    if enabled {
        writeln!(output, "NATIVE_LIFECYCLE stage={stage:?}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_output_is_opt_in_and_write_failures_are_returned() {
        let mut closed = io::Cursor::new([]);
        write_event(false, Stage::WindowClosed, &mut closed).unwrap();
        assert!(write_event(true, Stage::WindowClosed, &mut closed).is_err());
        let mut output = Vec::new();
        write_event(true, Stage::WindowClosed, &mut output).unwrap();
        write_event(true, Stage::EventLoopExited, &mut output).unwrap();
        assert_eq!(
            output,
            b"NATIVE_LIFECYCLE stage=WindowClosed\nNATIVE_LIFECYCLE stage=EventLoopExited\n"
        );
    }
}
