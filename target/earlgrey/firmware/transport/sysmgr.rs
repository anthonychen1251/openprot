// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use earlgrey_sysmgr_server::SysmgrServer;
use hal_flash::Flash;
use pw_status::Error;
use services_flash_client::FlashIpcClient;
use sysmgr_codegen::handle;
use userspace::process_entry;
use userspace::syscall::{self, Signals};
use userspace::time::Instant;
use util_error::{AsStatus, ErrorCode};
use util_io::RandomRead;
use util_ipc::IpcHandle;
use util_zfmt::messages::{ProcessExit, ProcessStart};
use zfmt::Zfmt;

#[derive(Zfmt)]
#[zfmt(format = "SPI Flash detected. Size: {size} bytes")]
struct SpiFlashDetected {
    size: u32,
}

#[derive(Zfmt)]
#[zfmt(
    format = "Update found! Staging slot: {staging_slot:c}, ROM_EXT offset: 0x{rom_ext_offset:x}, Owner offset: 0x{owner_offset:x}"
)]
struct UpdateTargetMapped {
    staging_slot: u32,
    rom_ext_offset: u32,
    owner_offset: u32,
}

fn sysmgr_server() -> Result<(), ErrorCode> {
    // SysmgrServer::new() will read boot log from retram and log boot info.
    let mut server = SysmgrServer::new()?;

    // Connect to SPI Flash service on flash_server
    let spi_flash_handle = IpcHandle::new(handle::SPI_FLASH_SYSMGR);
    if let Ok(mut spi_flash_client) = FlashIpcClient::new(spi_flash_handle) {
        let mut reader = spi_flash_client.random_reader();
        if let Ok(Some(_bundle)) = earlgrey_sysmgr_server::updater::scan_external_flash(&mut reader)
        {
            let size = reader.size()?;
            util_zfmt::info!(SpiFlashDetected { size: size as u32 });

            let current_slot = server.info.rom_ext.boot_slot;
            if let Some(remapper) = earlgrey_sysmgr_server::updater::StagingSlotRemapper::new(
                current_slot,
                server.info.app.boot_slot,
            ) {
                util_zfmt::info!(UpdateTargetMapped {
                    staging_slot: remapper.rom_ext_staging_slot.0,
                    rom_ext_offset: remapper.rom_ext_offset(),
                    owner_offset: remapper.owner_offset(),
                });
            }
        }
    }

    // ASSUMPTION: Pinmux settings for SPI Host 0 are assumed to be pre-configured by
    // the ROM_EXT stage or hardware/FPGA defaults. If migrating to a new development
    // platform in the future, explicit pinmux setup may need to be added here.

    let service_channel = IpcHandle::new(handle::SYSMGR_SERVICE);
    let mut buf = [0u8; 1024];

    loop {
        // Wait for incoming IPC request.
        syscall::object_wait(handle::SYSMGR_SERVICE, Signals::READABLE, Instant::MAX)
            .map_err(ErrorCode::kernel_error)?;

        // Process request.
        server.handle_one(&service_channel, &mut buf)?;
    }
}

#[process_entry("sysmgr")]
fn entry() -> Result<(), Error> {
    util_zfmt::info!(ProcessStart { name: "sysmgr" });
    let ret = sysmgr_server();
    util_zfmt::error!(ProcessExit {
        name: "sysmgr",
        status: ret.as_status()
    });
    Err(Error::Unknown)
}
