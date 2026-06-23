// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

use earlgrey_util::tags::{BootSlot, ManifestIdentifier};
use util_error::ErrorCode;
use util_io::RandomRead;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C)]
pub struct Manifest {
    pub signature: [u8; 816],
    pub address_translation: u32,
    pub identifier: ManifestIdentifier,
    pub manifest_version: u32,
    pub signed_region_end: u32,
    pub length: u32,
    pub version_major: u32,
    pub version_minor: u32,
    pub security_version: u32,
    pub timestamp: u64,
    pub binding_value: [u8; 32],
    pub max_key_version: u32,
    pub code_start: u32,
    pub code_end: u32,
    pub entry_point: u32,
    pub _pad: [u8; 120],
}

impl Manifest {
    pub fn new_zeroed() -> Self {
        Self {
            signature: [0u8; 816],
            address_translation: 0,
            identifier: ManifestIdentifier(0),
            manifest_version: 0,
            signed_region_end: 0,
            length: 0,
            version_major: 0,
            version_minor: 0,
            security_version: 0,
            timestamp: 0,
            binding_value: [0u8; 32],
            max_key_version: 0,
            code_start: 0,
            code_end: 0,
            entry_point: 0,
            _pad: [0u8; 120],
        }
    }
}

pub struct FirmwareBundle {
    pub offset: usize,
    pub rom_ext_len: usize,
    pub owner_len: usize,
}

pub fn scan_external_flash(
    flash: &mut impl RandomRead<Error = ErrorCode>,
) -> Result<Option<FirmwareBundle>, ErrorCode> {
    let flash_size = flash.size()?;
    let step = 64 * 1024; // 64kiB

    let mut offset = 0;
    while offset + 128 * 1024 <= flash_size {
        // 1. Read ROM_EXT candidate manifest
        let mut rom_ext_hdr = Manifest::new_zeroed();
        if flash.read(offset, rom_ext_hdr.as_mut_bytes()).is_ok() {
            if rom_ext_hdr.identifier == ManifestIdentifier::ROM_EXT {
                let rom_ext_len = rom_ext_hdr.length as usize;
                if rom_ext_len <= 64 * 1024 {
                    // 2. Read Owner candidate manifest at offset + 64kiB
                    let mut owner_hdr = Manifest::new_zeroed();
                    if flash
                        .read(offset + 64 * 1024, owner_hdr.as_mut_bytes())
                        .is_ok()
                    {
                        if owner_hdr.identifier == ManifestIdentifier::APPLICATION {
                            let owner_len = owner_hdr.length as usize;
                            return Ok(Some(FirmwareBundle {
                                offset,
                                rom_ext_len,
                                owner_len,
                            }));
                        }
                    }
                }
            }
        }
        offset += step;
    }

    Ok(None)
}

pub struct StagingSlotRemapper {
    pub rom_ext_staging_slot: BootSlot,
    pub owner_staging_slot: BootSlot,
}

impl StagingSlotRemapper {
    pub fn new(rom_ext_boot_slot: BootSlot, app_boot_slot: BootSlot) -> Option<Self> {
        let rom_ext_staging_slot = rom_ext_boot_slot.opposite()?;
        let owner_staging_slot = app_boot_slot.opposite()?;
        Some(Self {
            rom_ext_staging_slot,
            owner_staging_slot,
        })
    }

    pub fn rom_ext_offset(&self) -> u32 {
        match self.rom_ext_staging_slot {
            BootSlot::SlotA => 0,
            BootSlot::SlotB => 0x80000,
            _ => unreachable!(),
        }
    }

    pub fn owner_offset(&self) -> u32 {
        match self.owner_staging_slot {
            BootSlot::SlotA => 0x10000, // Slot A Owner partition starts at 64kiB
            BootSlot::SlotB => 0x90000, // Slot B Owner partition starts at 512kiB + 64kiB = 576kiB
            _ => unreachable!(),
        }
    }
}
