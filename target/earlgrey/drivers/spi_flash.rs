// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! SPI Flash driver for OpenTitan Earlgrey.
//!
//! This driver manages high-level SPI flash operations such as reading the
//! JEDEC ID, auto-detecting capacity, and reading data blocks.

#![cfg_attr(not(test), no_std)]

use earlgrey_spi_host::{SpiHost, SpiOpts, SpiTxnWidth};
use util_error::{ErrorCode, ErrorModule};
use util_io::RandomRead;

const OP_READ: u8 = 0x03;
const OP_READ4B: u8 = 0x13;
const OP_READ_JEDEC_ID: u8 = 0x9f;

/// The error module for the SPI Flash driver.
const FLASH_MODULE: ErrorModule = ErrorModule::new(0x464c); // ascii: FL (Flash)

pub struct SpiFlash {
    host: SpiHost,
    size: usize,
    use_4b_addressing: bool,
}

impl SpiFlash {
    /// Creates a new `SpiFlash` instance by auto-detecting the flash capacity via JEDEC ID.
    pub fn new(host: SpiHost) -> Result<Self, ErrorCode> {
        let mut jedec_id = [0u8; 3];
        let opts = SpiOpts {
            width: SpiTxnWidth::STANDARD,
            hold_cs: false,
        };

        // Read the 3-byte JEDEC ID: [Manufacturer, Memory Type, Capacity]
        host.write(
            &[OP_READ_JEDEC_ID],
            &SpiOpts {
                width: SpiTxnWidth::STANDARD,
                hold_cs: true,
            },
        )?;
        host.read(&mut jedec_id, &opts)?;

        let capacity_code = jedec_id[2];

        // Support both standard JEDEC capacity exponent codes (e.g. Winbond 0x14-0x1d)
        // and Macronix-style density codes (0x35-0x3c).
        let size = if (0x14..=0x1d).contains(&capacity_code) {
            1 << capacity_code
        } else if (0x35..=0x3c).contains(&capacity_code) {
            1 << (capacity_code - 32)
        } else {
            return Err(FLASH_MODULE.from_pw(1, pw_status::Error::NotFound));
        };

        let use_4b_addressing = size > 16 * 1024 * 1024; // > 16MB requires 4-byte addressing

        Ok(Self {
            host,
            size,
            use_4b_addressing,
        })
    }

    /// Returns the detected flash size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Reads data from the SPI flash into the provided buffer.
    pub fn read_data(&self, addr: usize, buf: &mut [u8]) -> Result<(), ErrorCode> {
        if addr + buf.len() > self.size {
            return Err(FLASH_MODULE.from_pw(2, pw_status::Error::OutOfRange));
        }

        let mut cmd_buf = [0u8; 5];
        let cmd_len = if self.use_4b_addressing {
            cmd_buf[0] = OP_READ4B;
            cmd_buf[1] = (addr >> 24) as u8;
            cmd_buf[2] = (addr >> 16) as u8;
            cmd_buf[3] = (addr >> 8) as u8;
            cmd_buf[4] = addr as u8;
            5
        } else {
            cmd_buf[0] = OP_READ;
            cmd_buf[1] = (addr >> 16) as u8;
            cmd_buf[2] = (addr >> 8) as u8;
            cmd_buf[3] = addr as u8;
            4
        };

        // Write the read command and address, holding Chip Select active.
        self.host.write(
            &cmd_buf[..cmd_len],
            &SpiOpts {
                width: SpiTxnWidth::STANDARD,
                hold_cs: true,
            },
        )?;

        // Read the requested bytes, releasing Chip Select at the end.
        self.host.read(
            buf,
            &SpiOpts {
                width: SpiTxnWidth::STANDARD,
                hold_cs: false,
            },
        )?;

        Ok(())
    }
}

impl RandomRead for SpiFlash {
    type Error = ErrorCode;

    fn read(&mut self, start_addr: usize, dst: &mut [u8]) -> Result<(), Self::Error> {
        self.read_data(start_addr, dst)
    }

    fn size(&mut self) -> Result<usize, Self::Error> {
        Ok(self.size)
    }
}
