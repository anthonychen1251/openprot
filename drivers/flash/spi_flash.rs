// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! SPI Flash driver.
//!
//! Implements `hal::blocking::flash::FlashDriver` for generic SPI NOR Flash chips
//! using `embedded-hal` SPI interfaces and SFDP discovery.

#![no_std]
#![allow(clippy::unnecessary_cast)]

use core::num::NonZero;
use hal_flash_driver::FlashAddress;
use util_error as error;
use util_error::ErrorCode;
use util_io::RandomRead;
use util_types::{Blocking, PowerOf2Usize};

const OP_WRITE_EN: u8 = 0x06;
const OP_READ_STATUS: u8 = 0x05;
const STATUS_WIP_MASK: u8 = 0x01;

#[derive(Clone, Copy)]
struct SfCmd {
    opcode: u8,
    addr_bytes: u8,
    dummy_bytes: u8,
}

/// SPI Flash driver.
pub struct SpiFlash<S: embedded_hal::spi::SpiDevice, B: Blocking> {
    spi: S,
    blocking: B,
    size_bytes: usize,
    erasable_sizes_bitmap: u32,
    read_cmd: SfCmd,
    program_cmd: SfCmd,
    erase_4k_opcode: u8,
    erase_64k_opcode: u8,
    addr_mode_4b: bool,
    initialized: bool,
}

impl<S: embedded_hal::spi::SpiDevice, B: Blocking> SpiFlash<S, B> {
    /// Creates a new, uninitialized `SpiFlash` driver.
    ///
    /// Must call `init()` before performing any flash operations.
    pub fn new(spi: S, blocking: B) -> Self {
        Self {
            spi,
            blocking,
            size_bytes: 0,
            erasable_sizes_bitmap: 0,
            read_cmd: SfCmd {
                opcode: 0,
                addr_bytes: 0,
                dummy_bytes: 0,
            },
            program_cmd: SfCmd {
                opcode: 0,
                addr_bytes: 0,
                dummy_bytes: 0,
            },
            erase_4k_opcode: 0,
            erase_64k_opcode: 0,
            addr_mode_4b: false,
            initialized: false,
        }
    }

    /// Initializes the driver by reading and parsing SFDP from the chip.
    pub fn init(&mut self) -> Result<(), ErrorCode> {
        let sfdp_reader = SfdpPhysicalReader::new(&mut self.spi);
        let mut sfdp = util_sfdp::SfdpReader::new(sfdp_reader)
            .map_err(|_| error::FLASH_GENERIC_SFDP_INVALID_SIGNATURE)?;

        let table = sfdp
            .basic_flash_parameters()
            .map_err(|_| error::FLASH_GENERIC_SFDP_PARAMETERS_TOO_SHORT)?;

        let size = table
            .table_jesd216()
            .memory_density
            .byte_len()
            .map_err(|_| error::FLASH_GENERIC_SFDP_INVALID_MEMORY_DENSITY)?;

        self.size_bytes = size as usize;
        self.erasable_sizes_bitmap = (1 << 12) | (1 << 16); // Support 4KB (bit 12) and 64KB (bit 16)

        // Verify page size if JESD216A+ is supported
        if let Some(jesd216a) = table.table_jesd216a() {
            let page_size = jesd216a.word11.page_size().value();
            if page_size != 256 {
                pw_log::error!(
                    "Unsupported flash page size: {} (expected 256)",
                    page_size as u32
                );
                return Err(error::FLASH_GENERIC_INVALID_PAGE_SIZE);
            }
        }

        // Configure opcodes based on capacity (explicitly enter 4-byte mode if > 16MB)
        if size <= 16 * 1024 * 1024 {
            self.read_cmd = SfCmd {
                opcode: 0x03, // Read Data
                addr_bytes: 3,
                dummy_bytes: 0,
            };
            self.program_cmd = SfCmd {
                opcode: 0x02, // Page Program
                addr_bytes: 3,
                dummy_bytes: 0,
            };
            self.erase_4k_opcode = 0x20; // Sector Erase 4KB
            self.erase_64k_opcode = 0xD8; // Block Erase 64KB
            self.addr_mode_4b = false;
        } else {
            // Explicitly enter 4-byte address mode
            self.enter_4byte_mode()?;

            // Standard read/write opcodes now expect 4-byte addresses
            self.read_cmd = SfCmd {
                opcode: 0x03, // Read Data (4B address)
                addr_bytes: 4,
                dummy_bytes: 0,
            };
            self.program_cmd = SfCmd {
                opcode: 0x02, // Page Program (4B address)
                addr_bytes: 4,
                dummy_bytes: 0,
            };
            self.erase_4k_opcode = 0x20; // Sector Erase 4KB (4B address)
            self.erase_64k_opcode = 0xD8; // Block Erase 64KB (4B address)
            self.addr_mode_4b = true;
        }

        self.initialized = true;
        pw_log::info!(
            "SPI Flash initialized: size={} bytes, 4B_mode={}",
            size as u32,
            self.addr_mode_4b as u32
        );
        Ok(())
    }

    fn enter_4byte_mode(&mut self) -> Result<(), ErrorCode> {
        self.write_enable()?;
        self.spi
            .write(&[0xB7]) // EN4B: Enter 4-Byte Address Mode
            .map_err(|_| error::FLASH_GENERIC_BUSY)?;
        Ok(())
    }

    fn write_enable(&mut self) -> Result<(), ErrorCode> {
        self.spi
            .write(&[OP_WRITE_EN])
            .map_err(|_| error::FLASH_GENERIC_BUSY)?;
        Ok(())
    }

    pub fn read_status(&mut self) -> Result<u8, ErrorCode> {
        let tx = [OP_READ_STATUS];
        let mut rx = [0u8];
        let mut ops = [
            embedded_hal::spi::Operation::Write(&tx),
            embedded_hal::spi::Operation::Read(&mut rx),
        ];
        self.spi
            .transaction(&mut ops)
            .map_err(|_| error::FLASH_GENERIC_BUSY)?;
        let [val] = rx;
        Ok(val)
    }

    fn is_busy_internal(&mut self) -> bool {
        if !self.initialized {
            return false;
        }
        match self.read_status() {
            Ok(status) => (status & STATUS_WIP_MASK) != 0,
            Err(_) => true,
        }
    }

    fn format_cmd_prefix(opcode: u8, addr: u32, addr_bytes: u8) -> ([u8; 5], usize) {
        let [b0, b1, b2, b3] = addr.to_be_bytes();
        if addr_bytes == 3 {
            ([opcode, b1, b2, b3, 0], 4)
        } else {
            ([opcode, b0, b1, b2, b3], 5)
        }
    }
}

impl<S: embedded_hal::spi::SpiDevice, B: Blocking> hal_flash::Flash for SpiFlash<S, B> {
    type Error = ErrorCode;

    fn geometry(&mut self) -> Result<(NonZero<usize>, PowerOf2Usize, u32), ErrorCode> {
        if !self.initialized {
            return Err(error::FLASH_GENERIC_NOT_INITIALIZED);
        }
        let bitmap = self.erasable_sizes_bitmap;
        let page_size = PowerOf2Usize::new(1 << (bitmap.trailing_zeros()))
            .ok_or(error::FLASH_GENERIC_NOT_INITIALIZED)?;
        let size = NonZero::new(self.size_bytes).ok_or(error::FLASH_GENERIC_NOT_INITIALIZED)?;
        Ok((size, page_size, bitmap))
    }

    fn read(&mut self, start_addr: FlashAddress, buf: &mut [u8]) -> Result<(), ErrorCode> {
        if !self.initialized {
            return Err(error::FLASH_GENERIC_NOT_INITIALIZED);
        }
        if buf.is_empty() {
            return Ok(());
        }

        let offset = start_addr.offset() as usize;
        let end_offset = offset
            .checked_add(buf.len())
            .ok_or(error::FLASH_GENERIC_ADDR_OUT_OF_BOUNDS)?;
        if end_offset > self.size_bytes {
            return Err(error::FLASH_GENERIC_ADDR_OUT_OF_BOUNDS);
        }

        let (mut prefix, mut len) = Self::format_cmd_prefix(
            self.read_cmd.opcode,
            start_addr.offset(),
            self.read_cmd.addr_bytes,
        );

        for _ in 0..self.read_cmd.dummy_bytes {
            if let Some(cell) = prefix.get_mut(len) {
                *cell = 0;
                len = len
                    .checked_add(1)
                    .ok_or(error::FLASH_GENERIC_BAD_ALIGNMENT)?;
            }
        }

        let mut ops = [
            embedded_hal::spi::Operation::Write(
                prefix
                    .get(..len)
                    .ok_or(error::FLASH_GENERIC_BAD_ALIGNMENT)?,
            ),
            embedded_hal::spi::Operation::Read(buf),
        ];

        self.spi
            .transaction(&mut ops)
            .map_err(|_| error::FLASH_GENERIC_BUSY)?;
        Ok(())
    }

    fn erase(&mut self, start_addr: FlashAddress, size: PowerOf2Usize) -> Result<(), ErrorCode> {
        if !self.initialized {
            return Err(error::FLASH_GENERIC_NOT_INITIALIZED);
        }

        let offset = start_addr.offset() as usize;
        if offset % size.get() != 0 {
            return Err(error::FLASH_GENERIC_ERASE_INVALID_ADDR);
        }
        let end_offset = offset
            .checked_add(size.get())
            .ok_or(error::FLASH_GENERIC_ADDR_OUT_OF_BOUNDS)?;
        if end_offset > self.size_bytes {
            return Err(error::FLASH_GENERIC_ADDR_OUT_OF_BOUNDS);
        }

        let size_log2 = size.get().trailing_zeros() as u8;
        let opcode = match size_log2 {
            12 => self.erase_4k_opcode,  // 4KB Erase
            16 => self.erase_64k_opcode, // 64KB Erase
            _ => return Err(error::FLASH_GENERIC_ERASE_INVALID_SIZE),
        };

        self.write_enable()?;

        let (prefix, len) = Self::format_cmd_prefix(
            opcode,
            start_addr.offset(),
            if self.addr_mode_4b { 4 } else { 3 },
        );

        self.spi
            .write(
                prefix
                    .get(..len)
                    .ok_or(error::FLASH_GENERIC_BAD_ALIGNMENT)?,
            )
            .map_err(|_| error::FLASH_GENERIC_BUSY)?;

        // Synchronous polling wait
        while self.is_busy_internal() {
            self.blocking.wait_for_notification();
        }

        Ok(())
    }

    fn program(&mut self, start_addr: FlashAddress, mut data: &[u8]) -> Result<(), ErrorCode> {
        if !self.initialized {
            return Err(error::FLASH_GENERIC_NOT_INITIALIZED);
        }
        if data.is_empty() {
            return Ok(());
        }

        const PROGRAM_WINDOW_SIZE: usize = 256;
        let window_mask = PROGRAM_WINDOW_SIZE - 1;
        let mut addr = start_addr;

        while !data.is_empty() {
            let chunk_len = core::cmp::min(
                data.len(),
                PROGRAM_WINDOW_SIZE - ((addr.offset() & window_mask as u32) as usize),
            );
            let chunk = data
                .get(..chunk_len)
                .ok_or(error::FLASH_GENERIC_BAD_ALIGNMENT)?;

            // Start program chunk
            self.write_enable()?;

            let (prefix, len) = Self::format_cmd_prefix(
                self.program_cmd.opcode,
                addr.offset(),
                self.program_cmd.addr_bytes,
            );

            let mut ops = [
                embedded_hal::spi::Operation::Write(
                    prefix
                        .get(..len)
                        .ok_or(error::FLASH_GENERIC_BAD_ALIGNMENT)?,
                ),
                embedded_hal::spi::Operation::Write(chunk),
            ];

            self.spi
                .transaction(&mut ops)
                .map_err(|_| error::FLASH_GENERIC_BUSY)?;

            // Synchronous polling wait for chunk completion
            while self.is_busy_internal() {
                self.blocking.wait_for_notification();
            }

            data = data
                .get(chunk.len()..)
                .ok_or(error::FLASH_GENERIC_BAD_ALIGNMENT)?;
            addr += chunk.len();
        }

        Ok(())
    }
}

/// Helper structure to read SFDP raw bytes from physical SPI flash.
struct SfdpPhysicalReader<'a, S: embedded_hal::spi::SpiDevice> {
    spi: &'a mut S,
}

impl<'a, S: embedded_hal::spi::SpiDevice> SfdpPhysicalReader<'a, S> {
    fn new(spi: &'a mut S) -> Self {
        Self { spi }
    }
}

impl<S: embedded_hal::spi::SpiDevice> RandomRead for SfdpPhysicalReader<'_, S> {
    type Error = ErrorCode;

    fn read(&mut self, start_addr: usize, dst: &mut [u8]) -> Result<(), Self::Error> {
        let [_, b1, b2, b3] = (start_addr as u32).to_be_bytes();
        let prefix = [
            0x5A, // OP_SFDP_READ
            b1, b2, b3, 0, // Dummy byte
        ];

        let mut ops = [
            embedded_hal::spi::Operation::Write(&prefix),
            embedded_hal::spi::Operation::Read(dst),
        ];

        self.spi
            .transaction(&mut ops)
            .map_err(|_| error::FLASH_GENERIC_BUSY)?;
        Ok(())
    }

    fn size(&mut self) -> Result<usize, Self::Error> {
        // SFDP space size is not strictly defined but 1<<24 (16MB) is a safe maximum limit.
        Ok(1 << 24)
    }
}
