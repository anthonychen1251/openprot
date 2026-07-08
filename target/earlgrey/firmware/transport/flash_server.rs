// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use flash_server_codegen::{handle, signals};
use pw_status::Error;
use userspace::time::{sleep_until, Clock, Duration, Instant, SystemClock};
use userspace::{process_entry, syscall};
use util_error::{AsStatus, ErrorCode, KERNEL_ERROR_INTERNAL};
use util_zfmt::messages::{ProcessExit, ProcessStart};

use earlgrey_util::EarlgreyFlashAddress;
use eflash_driver::{EmbeddedFlash, Permission};
use hal_flash::{BlockingFlash, FlashAddress};
use services_flash_server::FlashIpcServer;
use spi_flash::SpiFlash;
use spi_host::SpiHost0;
use util_ipc::IpcHandle;
use util_types::Blocking;

struct FlashCtrlInterrupt;

impl Blocking for FlashCtrlInterrupt {
    fn wait_for_notification(&self) {
        loop {
            if let Ok(w) = syscall::object_wait(
                handle::FLASH_INTERRUPTS,
                signals::FLASH_CTRL_OP_DONE,
                Instant::MAX,
            ) {
                if w.pending_signals.contains(signals::FLASH_CTRL_OP_DONE) {
                    break;
                }
            }
        }
        let _ = syscall::interrupt_ack(handle::FLASH_INTERRUPTS, signals::FLASH_CTRL_OP_DONE);
    }
}

struct SpiFlashSleep;

impl Blocking for SpiFlashSleep {
    fn wait_for_notification(&self) {
        let _ = sleep_until(SystemClock::now() + Duration::from_millis(1));
    }
}

fn flash_server() -> Result<(), ErrorCode> {
    let mut eflash_driver =
        EmbeddedFlash::new_with_interrupts(unsafe { flash_ctrl_core::FlashCtrl::new() });
    eflash_driver.set_default_permission(Permission::FULL_ACCESS);
    for i in 5..9 {
        eflash_driver.set_info_permission(FlashAddress::info(0, i, 0), Permission::FULL_ACCESS)?;
        eflash_driver.set_info_permission(FlashAddress::info(1, i, 0), Permission::FULL_ACCESS)?;
    }
    let eflash = BlockingFlash {
        driver: eflash_driver,
        blocking: FlashCtrlInterrupt,
    };
    let mut eflash_server = FlashIpcServer::new(eflash);

    let spi_host0 = unsafe { SpiHost0::new() };
    let mut spi_host = earlgrey_spi_host::SpiHost::new(spi_host0);
    spi_host.init().map_err(|_| KERNEL_ERROR_INTERNAL)?;

    let blocking = SpiFlashSleep;
    let mut spi_flash = SpiFlash::new(spi_host, blocking);
    spi_flash.init().map_err(|_| KERNEL_ERROR_INTERNAL)?;
    let mut spi_flash_server = FlashIpcServer::new(spi_flash);

    syscall::wait_group_add(
        handle::FLASH_WAIT_GROUP,
        handle::FLASH_SERVICE,
        syscall::Signals::READABLE,
        1, // token 1 = EFlash
    )
    .map_err(ErrorCode::kernel_error)?;

    syscall::wait_group_add(
        handle::FLASH_WAIT_GROUP,
        handle::SPI_FLASH_SERVICE,
        syscall::Signals::READABLE,
        2, // token 2 = SPI Flash
    )
    .map_err(ErrorCode::kernel_error)?;

    let mut buf = [0u8; 2064];
    let eflash_ipc = IpcHandle::new(handle::FLASH_SERVICE);
    let spi_flash_ipc = IpcHandle::new(handle::SPI_FLASH_SERVICE);
    loop {
        let wait_result = syscall::object_wait(
            handle::FLASH_WAIT_GROUP,
            syscall::Signals::READABLE,
            Instant::MAX,
        )
        .map_err(ErrorCode::kernel_error)?;

        let token = wait_result.user_data;
        if token == 1 {
            eflash_server.handle_one(&eflash_ipc, &mut buf)?;
        } else if token == 2 {
            spi_flash_server.handle_one(&spi_flash_ipc, &mut buf)?;
        }
    }
}

#[process_entry("flash_server")]
fn entry() -> Result<(), Error> {
    util_zfmt::info!(ProcessStart {
        name: "flash_server"
    });
    let ret = flash_server();
    util_zfmt::error!(ProcessExit {
        name: "flash_server",
        status: ret.as_status()
    });

    Err(Error::Unknown)
}
