// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use pw_status::Error;
use userspace::time::{sleep_until, Clock, Duration, SystemClock};
use userspace::{process_entry, syscall};
use util_error::{AsStatus, ErrorCode};
use util_zfmt::messages::{ProcessExit, ProcessStart};

use platform_config::{EarlGreyGpio, PinmuxConfig, PlatformConfig};

/*
 * TODO: implement platform server.
 */

fn platform_server() -> Result<(), ErrorCode> {
    // Safety: Exclusive access to GPIO and Pinmux hardware peripherals is guaranteed
    // within the platform process memory mapping in system.json5.
    let mut gpio = unsafe { EarlGreyGpio::new() };
    let platform_config = PlatformConfig::default();
    platform_config.pinmux_config(&mut gpio);

    loop {
        sleep_until(SystemClock::now() + Duration::from_secs(600))
            .map_err(ErrorCode::kernel_error)?;
    }
}

#[process_entry("platform")]
fn entry() -> Result<(), Error> {
    util_zfmt::info!(ProcessStart { name: "platform" });
    let ret = platform_server();
    util_zfmt::error!(ProcessExit {
        name: "platform",
        status: ret.as_status()
    });

    let status_res = ret.map_err(|_| Error::Unknown);
    syscall::debug_shutdown(status_res)
}
