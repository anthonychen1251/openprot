// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use earlgrey_platform_server::PlatformServer;
use platform_codegen::handle;
use platform_config::{EarlGreyGpio, PinmuxConfig, PlatformConfig};
use pw_status::Error;
use userspace::process_entry;
use userspace::syscall::{self, Signals};
use userspace::time::Instant;
use util_error::{AsStatus, ErrorCode};
use util_ipc::IpcHandle;
use util_zfmt::messages::{ProcessExit, ProcessStart};

fn platform_server() -> Result<(), ErrorCode> {
    // Safety: Exclusive access to GPIO and Pinmux hardware peripherals is guaranteed
    // within the platform process memory mapping in system.json5.
    let mut gpio = unsafe { EarlGreyGpio::new() };
    let platform_config = PlatformConfig::default();
    platform_config.pinmux_config(&mut gpio);

    let mut server = PlatformServer::new(gpio);

    syscall::wait_group_add(
        handle::PLATFORM_WAIT_GROUP,
        handle::PLATFORM_UPDATER_SERVICE,
        Signals::READABLE,
        1,
    )
    .map_err(ErrorCode::kernel_error)?;

    syscall::wait_group_add(
        handle::PLATFORM_WAIT_GROUP,
        handle::GPIO_INTERRUPTS,
        Signals::READABLE,
        2,
    )
    .map_err(ErrorCode::kernel_error)?;

    let updater_channel = IpcHandle::new(handle::PLATFORM_UPDATER_SERVICE);
    let mut buf = [0u8; 1024];

    loop {
        let wait_result =
            syscall::object_wait(handle::PLATFORM_WAIT_GROUP, Signals::READABLE, Instant::MAX)
                .map_err(ErrorCode::kernel_error)?;

        if wait_result.user_data == 1 {
            server.handle_one(&updater_channel, &mut buf)?;
        } else if wait_result.user_data == 2 {
            if server.handle_gpio_interrupt()? {
                server.respond_pending_usb_wait(&updater_channel)?;
            }
        }
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
