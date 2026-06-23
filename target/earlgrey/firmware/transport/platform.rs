// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use earlgrey_util::error::{
    EG_ERROR_SPI_MUX_CTRL_CONFIG_FAILED, EG_ERROR_SPI_MUX_OE_CONFIG_FAILED,
    EG_ERROR_SPI_MUX_SET_FAILED,
};
use openprot_hal_blocking::gpio_port::GpioPort;
use pw_status::Error;
use userspace::time::{sleep_until, Clock, Duration, SystemClock};
use userspace::{process_entry, syscall};
use util_error::{AsStatus, ErrorCode};
use util_zfmt::messages::{ProcessExit, ProcessStart};
use zfmt::Zfmt;

#[derive(Zfmt)]
#[zfmt(format = "Failed to configure SPI Mux: 0x{status:08x}")]
struct SpiMuxSetupFailed {
    status: u32,
}

/// Configures the GPIO pins and Pinmux routing to connect Earlgrey's SPI Host 0
/// to the external SPI Flash 0 slot.
fn setup_spi_mux_for_flash0() -> Result<(), ErrorCode> {
    // Switch the SPI Mux to select external SPI Flash 0.
    // SPI_MUX_CTRL_GPIO = GpioPin::Pin8 -> Pad::IOA8 (selects Flash 0 when Low)
    // SPI_MUX_OE_GPIO = GpioPin::Pin9 -> Pad::IOB8 (enables Mux Output when Low)
    // Both pins must be configured as outputs with their respective pads routed.
    // SAFETY: We have exclusive access to the GPIO and Pinmux peripherals.
    let mut gpio_port = unsafe { earlgrey_gpio::EarlGreyGpio::new() };

    // Configure GPIO 8 (Mux Ctrl) routed to Pad IOA8
    let pin8_mask = earlgrey_gpio::GpioMask(1 << 8);
    let config8 = earlgrey_gpio::EarlGreyPinConfig {
        is_input: false,
        is_output: true,
        pad: Some(earlgrey_pinmux::Pad::IOA8),
        ..Default::default()
    };
    GpioPort::configure(&mut gpio_port, pin8_mask, config8)
        .map_err(|_| EG_ERROR_SPI_MUX_CTRL_CONFIG_FAILED)?;

    // Configure GPIO 9 (Mux OE) routed to Pad IOB8
    let pin9_mask = earlgrey_gpio::GpioMask(1 << 9);
    let config9 = earlgrey_gpio::EarlGreyPinConfig {
        is_input: false,
        is_output: true,
        pad: Some(earlgrey_pinmux::Pad::IOB8),
        ..Default::default()
    };
    GpioPort::configure(&mut gpio_port, pin9_mask, config9)
        .map_err(|_| EG_ERROR_SPI_MUX_OE_CONFIG_FAILED)?;

    // Set GPIO 8 (Mux Ctrl) to High (1) to connect the upstream device to Flash 1.
    // Set GPIO 9 (Mux OE) to Low (0) to enable Mux outputs.
    GpioPort::set_reset(
        &mut gpio_port,
        earlgrey_gpio::GpioMask(1 << 8), // set_mask (High)
        earlgrey_gpio::GpioMask(1 << 9), // reset_mask (Low)
    )
    .map_err(|_| EG_ERROR_SPI_MUX_SET_FAILED)?;

    Ok(())
}

fn platform_server() -> Result<(), ErrorCode> {
    // Manage the SPI multiplexor to present the correct SPI EEPROM to the system.
    if let Err(e) = setup_spi_mux_for_flash0() {
        util_zfmt::error!(SpiMuxSetupFailed { status: e.0.get() });
    }

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
