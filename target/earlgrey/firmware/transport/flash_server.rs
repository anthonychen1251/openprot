// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use flash_server_codegen::{handle, signals};
use pw_status::Error;
use userspace::time::Instant;
use userspace::{process_entry, syscall};
use util_error::{AsStatus, ErrorCode, KERNEL_ERROR_INTERNAL};
use util_zfmt::messages::{ProcessExit, ProcessStart};
use zfmt::Zfmt;

use earlgrey_util::EarlgreyFlashAddress;
use eflash_driver::{EmbeddedFlash, Permission};
use hal_flash::{BlockingFlash, Flash, FlashAddress};
use services_flash_server::FlashIpcServer;
use spi_flash::SpiFlash;
use spi_host::SpiHost0;
use util_ipc::IpcHandle;
use util_types::Blocking;

#[derive(Zfmt)]
#[zfmt(format = "SPI Host init failed: {code}")]
struct SpiHostInitFailed {
    code: u32,
}

#[derive(Zfmt)]
#[zfmt(format = "SPI Flash init failed: {code:08x}")]
struct SpiFlashInitFailed {
    code: u32,
}

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

use services_flash_opcode::{IPC_OP_FLASH_LOCK, IPC_OP_FLASH_UNLOCK};
use util_ipc::IpcChannel;
use zerocopy::IntoBytes;

#[derive(Zfmt)]
struct FlashChannel {
    handle: u32,
    token: usize,
    ipc: IpcHandle,
}

struct PendingWaiter {
    token: usize,
    handle: u32,
    ipc: IpcHandle,
}

struct FlashService<TFlash: Flash<Error = ErrorCode>> {
    server: FlashIpcServer<TFlash>,
    channels: &'static [FlashChannel],
    pending_lock: Option<PendingWaiter>,
}

impl<TFlash: Flash<Error = ErrorCode>> FlashService<TFlash> {
    fn dispatch(&mut self, current_token: usize, buf: &mut [u8]) -> Result<(), ErrorCode> {
        let current_channel = self
            .channels
            .iter()
            .find(|ch| ch.token == current_token)
            .ok_or(KERNEL_ERROR_INTERNAL)?;

        let (opcode, res) =
            self.server
                .handle_one_with_token(current_token, &current_channel.ipc, buf)?;

        if opcode == IPC_OP_FLASH_LOCK && res.is_err() {
            if self.pending_lock.is_some() {
                let status = util_error::FLASH_GENERIC_LOCKED.0.get();
                let _ = current_channel.ipc.respond(&[status.as_bytes(), &[]]);
                return Ok(());
            }

            // Failed blocking lock attempt from non-owner client:
            // Remove channel from WAIT_GROUP and save as pending lock waiter (response was deferred)
            let _ = syscall::wait_group_remove(handle::FLASH_WAIT_GROUP, current_channel.handle);
            self.pending_lock = Some(PendingWaiter {
                token: current_token,
                handle: current_channel.handle,
                ipc: current_channel.ipc,
            });
            return Ok(());
        }

        if opcode == IPC_OP_FLASH_UNLOCK && !self.server.is_locked() {
            if let Some(waiter) = self.pending_lock.take() {
                self.server.force_lock(waiter.token);
                let status = 0u32;
                let _ = waiter.ipc.respond(&[status.as_bytes(), &[]]);
                let _ = syscall::wait_group_add(
                    handle::FLASH_WAIT_GROUP,
                    waiter.handle,
                    syscall::Signals::READABLE,
                    waiter.token,
                );
            }
        }

        Ok(())
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

    static EFLASH_CHANNELS: &[FlashChannel] = &[
        FlashChannel {
            handle: handle::EFLASH_UPDATEMGR_SERVICE,
            token: 1,
            ipc: IpcHandle::new(handle::EFLASH_UPDATEMGR_SERVICE),
        },
        FlashChannel {
            handle: handle::EFLASH_USB_SERVICE,
            token: 2,
            ipc: IpcHandle::new(handle::EFLASH_USB_SERVICE),
        },
    ];

    let mut eflash_service = FlashService {
        server: FlashIpcServer::new(eflash),
        channels: EFLASH_CHANNELS,
        pending_lock: None,
    };

    let mmio0 = unsafe { spi_host::RegisterBlock::new(SpiHost0::PTR) };
    let mut spi_host = unsafe { earlgrey_spi_host::SpiHost::new(mmio0) };
    if let Err(e) = spi_host.init(&earlgrey_spi_host::SpiConfig::DEFAULT_SPI0) {
        let err_num = match e {
            earlgrey_spi_host::SpiError::InvalidTransaction => 1,
            earlgrey_spi_host::SpiError::FifoOverflow => 2,
            earlgrey_spi_host::SpiError::FifoUnderflow => 3,
            earlgrey_spi_host::SpiError::Timeout => 4,
            earlgrey_spi_host::SpiError::HardwareError => 5,
        };
        util_zfmt::error!(SpiHostInitFailed { code: err_num });
        return Err(KERNEL_ERROR_INTERNAL);
    }
    let mut spi_flash = SpiFlash::new(spi_host);
    if let Err(e) = spi_flash.init() {
        util_zfmt::error!(SpiFlashInitFailed { code: u32::from(e) });
        return Err(e);
    }

    static SPI_FLASH_CHANNELS: &[FlashChannel] = &[
        FlashChannel {
            handle: handle::SPI_FLASH_UPDATEMGR_SERVICE,
            token: 3,
            ipc: IpcHandle::new(handle::SPI_FLASH_UPDATEMGR_SERVICE),
        },
        FlashChannel {
            handle: handle::SPI_FLASH_USB_SERVICE,
            token: 4,
            ipc: IpcHandle::new(handle::SPI_FLASH_USB_SERVICE),
        },
    ];

    let mut spi_flash_service = FlashService {
        server: FlashIpcServer::new(spi_flash),
        channels: SPI_FLASH_CHANNELS,
        pending_lock: None,
    };

    for ch in EFLASH_CHANNELS.iter().chain(SPI_FLASH_CHANNELS.iter()) {
        syscall::wait_group_add(
            handle::FLASH_WAIT_GROUP,
            ch.handle,
            syscall::Signals::READABLE,
            ch.token,
        )
        .map_err(ErrorCode::kernel_error)?;
    }

    let mut buf = [0u8; 2064];

    loop {
        let wait_result = syscall::object_wait(
            handle::FLASH_WAIT_GROUP,
            syscall::Signals::READABLE,
            Instant::MAX,
        )
        .map_err(ErrorCode::kernel_error)?;

        let token = wait_result.user_data;
        match token {
            1 | 2 => eflash_service.dispatch(token, &mut buf)?,
            3 | 4 => spi_flash_service.dispatch(token, &mut buf)?,
            _ => {}
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
