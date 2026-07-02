// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

use earlgrey_spi_flash::SpiFlash;
use earlgrey_spi_host::SpiHost;
use earlgrey_sysmgr_server::SysmgrServer;
use earlgrey_util::flash::EarlgreyFlashAddress;
use earlgrey_util::tags::BootSlot;
use hal_flash::{Flash, FlashAddress};
use pw_status::Error;
use services_flash_client::FlashIpcClient;
use sysmgr_codegen::handle;
use userspace::process_entry;
use userspace::syscall::{self, Signals};
use userspace::time::{sleep_until, Clock, Duration, Instant, SystemClock};
use util_error::{AsStatus, ErrorCode};
use util_io::RandomRead;
use util_ipc::IpcHandle;
use util_zfmt::messages::{ProcessExit, ProcessStart};
use zerocopy::IntoBytes;
use zfmt::Zfmt;

#[derive(Zfmt)]
#[zfmt(format = "SPI Flash detected. Size: {size} bytes")]
struct SpiFlashDetected {
    size: u32,
}

#[derive(Zfmt)]
#[zfmt(
    format = "Update found! ROM_EXT staging: {rom_ext_staging_slot:c} (offset: 0x{rom_ext_offset:x}), Owner staging: {owner_staging_slot:c} (offset: 0x{owner_offset:x})"
)]
struct UpdateTargetMapped {
    rom_ext_staging_slot: u32,
    rom_ext_offset: u32,
    owner_staging_slot: u32,
    owner_offset: u32,
}

#[derive(Zfmt)]
#[zfmt(format = "Flashing {region} partition: EFLASH offset 0x{start:x} ({len} bytes)")]
struct FlashingRegion {
    region: &'static str,
    start: u32,
    len: u32,
}

#[derive(Zfmt)]
#[zfmt(format = "Successfully wrote {region} partition to EFLASH!")]
struct FlashWriteSuccess {
    region: &'static str,
}

#[derive(Zfmt)]
#[zfmt(format = "Failed to write {region} partition! Status: 0x{status:08x}")]
struct FlashWriteFailed {
    region: &'static str,
    status: u32,
}

#[derive(Zfmt)]
#[zfmt(format = "Firmware update installation complete! Rebooting into the new slot...")]
struct UpdateComplete {}

#[derive(Zfmt)]
#[zfmt(format = "HWE Application version: {major}.{minor}")]
struct HweVersionLog {
    major: u32,
    minor: u32,
}

#[derive(Zfmt)]
#[zfmt(format = "Owner block found at SPI Flash offset: 0x{offset:x}")]
struct OwnerBlockFound {
    offset: u32,
}

#[derive(Zfmt)]
#[zfmt(format = "Owner block NOT found on SPI Flash!")]
struct OwnerBlockNotFound {}

/// Helper function to erase and write a firmware partition page-by-page.
fn flash_write_partition(
    flash_client: &mut FlashIpcClient,
    spi_flash: &mut SpiFlash,
    src_offset: usize,
    dest_offset: u32,
    len: usize,
) -> Result<(), ErrorCode> {
    // Get EFLASH geometry to find page size
    let (_, page_size, _) = flash_client.geometry()?;
    let page_len = page_size.get();

    // 1. Erase the target partition region in EFLASH page-by-page
    let mut erased = 0;
    while erased < len {
        let erase_addr = dest_offset + erased as u32;
        flash_client.erase(FlashAddress::data(erase_addr), page_size)?;
        erased += page_len;
    }

    // 2. Read from external flash and program to EFLASH page-by-page
    let mut page_buf = [0u8; 2048]; // 2kiB buffer on the stack (safe with 8kiB stack room)
    let mut written = 0;
    while written < len {
        let chunk_len = core::cmp::min(len - written, page_len);
        let src_addr = src_offset + written;
        let dest_addr = dest_offset + written as u32;

        page_buf.fill(0);
        spi_flash.read(src_addr, &mut page_buf[..chunk_len])?;
        flash_client.program(FlashAddress::data(dest_addr), &page_buf)?;

        written += chunk_len;
    }

    Ok(())
}

/// Helper function to read the running HWE application's manifest version from internal EFLASH.
fn read_hwe_version(flash_client: &mut FlashIpcClient, boot_slot: BootSlot) -> Option<(u32, u32)> {
    use earlgrey_sysmgr_server::updater::Manifest;

    let offset = match boot_slot {
        BootSlot::SlotB => 0x90000u32,
        _ => 0x10000u32,
    };

    let mut manifest = Manifest::new_zeroed();
    if flash_client
        .read(FlashAddress::data(offset), manifest.as_mut_bytes())
        .is_ok()
    {
        Some((manifest.version_major, manifest.version_minor))
    } else {
        None
    }
}

const K_TLV_TAG_OWNER: u32 = 0x524e574f;

fn scan_owner_block(
    flash: &mut impl RandomRead<Error = ErrorCode>,
) -> Result<Option<usize>, ErrorCode> {
    let flash_size = flash.size()?;
    let page_size = 2048;
    let mut offset = 0;
    while offset + 2048 <= flash_size {
        let mut buf = [0u8; 8];
        if flash.read(offset, &mut buf).is_ok() {
            let tag = u32::from_le_bytes(buf[0..4].try_into().unwrap());
            let length = u16::from_le_bytes(buf[4..6].try_into().unwrap());
            let version_major = buf[6];
            if tag == K_TLV_TAG_OWNER && length == 2048 && version_major == 0 {
                return Ok(Some(offset));
            }
        }
        offset += page_size;
    }
    Ok(None)
}

fn sysmgr_server() -> Result<(), ErrorCode> {
    // SysmgrServer::new() will read boot log from retram and log boot info.
    let mut server = SysmgrServer::new()?;

    // Instantiate EFLASH client to read our own version and for future updates
    let flash_ipc_handle = IpcHandle::new(handle::FLASH_SYSMGR);
    let mut flash_client = FlashIpcClient::new(flash_ipc_handle).ok();

    // Read and print the HWE application version from its manifest
    if let Some(ref mut client) = flash_client {
        if let Some((major, minor)) = read_hwe_version(client, server.info.app.boot_slot) {
            util_zfmt::info!(HweVersionLog { major, minor });
        }
    }

    // Instantiate EFLASH client to read our own version and for future updates
    let flash_ipc_handle = IpcHandle::new(handle::FLASH_SYSMGR);
    let mut flash_client = FlashIpcClient::new(flash_ipc_handle).ok();

    // Read and print the HWE application version from its manifest
    if let Some(ref mut client) = flash_client {
        if let Some((major, minor)) = read_hwe_version(client, server.info.app.boot_slot) {
            util_zfmt::info!(HweVersionLog { major, minor });
        }
    }

    // Initialize SPI Host 0 and SPI Flash
    let spi_host = unsafe { SpiHost::new(0x40300000 as *mut u32) };
    if let Ok(mut spi_flash) = SpiFlash::new(spi_host) {
        if let Ok(Some(bundle)) =
            earlgrey_sysmgr_server::updater::scan_external_flash(&mut spi_flash)
        {
            let size = spi_flash.size();
            util_zfmt::info!(SpiFlashDetected { size: size as u32 });

            if let Some(remapper) = earlgrey_sysmgr_server::updater::StagingSlotRemapper::new(
                server.info.rom_ext.boot_slot,
                server.info.app.boot_slot,
            ) {
                let target_rom_ext = remapper.rom_ext_offset();
                let target_owner = remapper.owner_offset();

                util_zfmt::info!(UpdateTargetMapped {
                    rom_ext_staging_slot: remapper.rom_ext_staging_slot.0,
                    rom_ext_offset: target_rom_ext,
                    owner_staging_slot: remapper.owner_staging_slot.0,
                    owner_offset: target_owner,
                });

                // Reuse the EFLASH client we instantiated at boot!
                if let Some(ref mut client) = flash_client {
                    // 1. Write ROM_EXT
                    util_zfmt::info!(FlashingRegion {
                        region: "ROM_EXT",
                        start: target_rom_ext,
                        len: bundle.rom_ext_len as u32,
                    });
                    match flash_write_partition(
                        client,
                        &mut spi_flash,
                        bundle.offset,
                        target_rom_ext,
                        bundle.rom_ext_len,
                    ) {
                        Ok(()) => {
                            util_zfmt::info!(FlashWriteSuccess { region: "ROM_EXT" });

                            // 2. Write Owner software
                            util_zfmt::info!(FlashingRegion {
                                region: "Owner",
                                start: target_owner,
                                len: bundle.owner_len as u32,
                            });
                            match flash_write_partition(
                                client,
                                &mut spi_flash,
                                bundle.offset + 64 * 1024,
                                target_owner,
                                bundle.owner_len,
                            ) {
                                Ok(()) => {
                                    util_zfmt::info!(FlashWriteSuccess { region: "Owner" });

                                    match scan_owner_block(&mut spi_flash) {
                                        Ok(Some(offset)) => {
                                            util_zfmt::info!(OwnerBlockFound { offset: offset as u32 });
                                        }
                                        _ => {
                                            util_zfmt::warn!(OwnerBlockNotFound {});
                                        }
                                    }

                                    // 3. Update boot slot preference to Owner staging slot
                                    server.set_boot_policy(
                                        remapper.owner_staging_slot,
                                        remapper.owner_staging_slot,
                                    );
                                    util_zfmt::info!(UpdateComplete {});

                                    // 4. Trigger system reboot
                                    let _ = sleep_until(
                                        SystemClock::now() + Duration::from_millis(200),
                                    );
                                    server.request_reboot();
                                }
                                Err(e) => {
                                    util_zfmt::error!(FlashWriteFailed {
                                        region: "Owner",
                                        status: e.0.get(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            util_zfmt::error!(FlashWriteFailed {
                                region: "ROM_EXT",
                                status: e.0.get(),
                            });
                        }
                    }
                }
            }
        }
    }

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
