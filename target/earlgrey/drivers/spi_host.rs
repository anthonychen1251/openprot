// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! SPI Host driver for Earlgrey.
//!
//! This driver implements the `embedded-hal` 1.0 SPI traits for the Earlgrey SPI Host controller.

#![no_std]

/// SPI Host driver errors.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SpiError {
    /// Invalid transaction parameters or state.
    InvalidTransaction,
    /// TX FIFO overflow.
    FifoOverflow,
    /// RX FIFO underflow.
    FifoUnderflow,
    /// Operation timed out.
    Timeout,
    /// Hardware error reported by the controller.
    HardwareError,
}

impl embedded_hal::spi::Error for SpiError {
    fn kind(&self) -> embedded_hal::spi::ErrorKind {
        // Map all errors to Other for now.
        embedded_hal::spi::ErrorKind::Other
    }
}

/// Configuration for the SPI Host driver.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SpiConfig {
    /// Clock divider to adjust transfer speed.
    /// Formula: f_sck = f_core / (2 * (clkdiv + 1))
    pub clkdiv: u32,
    /// SPI clock polarity (false for low when idle, true for high when idle).
    pub cpol: bool,
    /// SPI clock phase (false to sample on first edge, true on second edge).
    pub cpha: bool,
    /// Chip Select ID line (0 for CS0, 1 for CS1, etc.)
    pub csid: u32,
}

/// Trait representing a SPI Host MMIO interface.
pub trait SpiHostMmio {
    /// Get the register block for reading.
    fn regs(&self) -> spi_host::RegisterBlock<ureg::RealMmio<'_>>;
    /// Get the register block for writing.
    fn regs_mut(&mut self) -> spi_host::RegisterBlock<ureg::RealMmioMut<'_>>;
}

impl SpiHostMmio for spi_host::SpiHost0 {
    fn regs(&self) -> spi_host::RegisterBlock<ureg::RealMmio<'_>> {
        self.regs()
    }
    fn regs_mut(&mut self) -> spi_host::RegisterBlock<ureg::RealMmioMut<'_>> {
        self.regs_mut()
    }
}

impl SpiHostMmio for spi_host::SpiHost1 {
    fn regs(&self) -> spi_host::RegisterBlock<ureg::RealMmio<'_>> {
        self.regs()
    }
    fn regs_mut(&mut self) -> spi_host::RegisterBlock<ureg::RealMmioMut<'_>> {
        self.regs_mut()
    }
}

/// Earlgrey SPI Host driver.
pub struct SpiHost<Mmio: SpiHostMmio> {
    mmio: Mmio,
}

impl<Mmio: SpiHostMmio> SpiHost<Mmio> {
    /// Create a new SpiHost instance using the peripheral ownership token.
    pub fn new(mmio: Mmio) -> Self {
        Self { mmio }
    }

    /// Dynamically reconfigure the SPI Host speed, mode, and chip select.
    pub fn configure(&mut self, config: &SpiConfig) -> Result<(), SpiError> {
        self.mmio.regs_mut().configopts().write(|w| {
            w.clkdiv(config.clkdiv)
                .cpol(config.cpol)
                .cpha(config.cpha)
                .fullcyc(true)
                .csnlead(0)
                .csnidle(2)
                .csntrail(1)
        });
        self.mmio.regs_mut().csid().write(|_| config.csid);
        Ok(())
    }

    /// Initialize the SPI Host peripheral.
    ///
    /// Configures the clock, SPI mode (0), CS timings, performs a reset,
    /// and enables the peripheral.
    pub fn init(&mut self) -> Result<(), SpiError> {
        // Default safe configuration (24MHz if core is 96MHz, Mode 0, CS0)
        let default_config = SpiConfig {
            clkdiv: 1,
            cpol: false,
            cpha: false,
            csid: 0,
        };
        self.configure(&default_config)?;

        // Reset the peripheral.
        self.mmio.regs_mut().control().write(|w| w.sw_rst(true));

        // Wait for both FIFOs to empty.
        let mut timeout = TIMEOUT_LIMIT;
        loop {
            let status = self.mmio.regs_mut().status().read();
            if status.txempty() && status.rxempty() {
                break;
            }
            timeout = timeout.checked_sub(1).ok_or(SpiError::Timeout)?;
        }

        // Release reset and enable SPI host.
        self.mmio
            .regs_mut()
            .control()
            .write(|w| w.sw_rst(false).spien(true));

        Ok(())
    }

    /// Check if the SPI Host is currently active processing a command.
    fn is_active(&self) -> bool {
        self.mmio.regs().status().read().active()
    }

    /// Send a write command segment.
    fn write_cmd(
        &mut self,
        mut data: &[u8],
        speed: u32,
        final_csaat: bool,
    ) -> Result<(), SpiError> {
        let regs = self.mmio.regs_mut();
        regs.control().modify(|w| w.output_en(true));

        while !data.is_empty() {
            let mut timeout = TIMEOUT_LIMIT;
            loop {
                let status = regs.status().read();
                if status.ready() && !status.active() {
                    break;
                }
                timeout = timeout.checked_sub(1).ok_or(SpiError::Timeout)?;
            }

            let mut txqd_timeout = TIMEOUT_LIMIT;
            while regs.status().read().txqd() != 0 {
                txqd_timeout = txqd_timeout.checked_sub(1).ok_or(SpiError::Timeout)?;
            }

            let chunk_len = core::cmp::min(data.len(), MAX_TX_CHUNK_LEN);
            let chunk = data.get(..chunk_len).ok_or(SpiError::InvalidTransaction)?;
            data = data.get(chunk_len..).ok_or(SpiError::InvalidTransaction)?;

            util_regcpy::copy_to_reg_unaligned(&regs.txdata(), chunk);

            let chunk_csaat = !data.is_empty() || final_csaat;

            regs.command().write(|w| {
                w.speed(speed)
                    .csaat(chunk_csaat)
                    .direction(DIR_TXONLY)
                    .len(chunk_len.saturating_sub(1) as u32)
            });
        }
        Ok(())
    }

    /// Send a read command segment and receive data into `dest`.
    fn read_cmd(
        &mut self,
        mut len: usize,
        speed: u32,
        final_csaat: bool,
        mut dest: &mut [u8],
    ) -> Result<(), SpiError> {
        let regs = self.mmio.regs_mut();
        regs.control().modify(|w| w.output_en(true));

        while len > 0 {
            let mut timeout = TIMEOUT_LIMIT;
            loop {
                let status = regs.status().read();
                if status.ready() && !status.active() {
                    break;
                }
                timeout = timeout.checked_sub(1).ok_or(SpiError::Timeout)?;
            }

            let chunk_len = core::cmp::min(len, MAX_RX_CHUNK_LEN);
            len -= chunk_len;

            let chunk_csaat = len > 0 || final_csaat;

            regs.command().write(|w| {
                w.speed(speed)
                    .csaat(chunk_csaat)
                    .direction(DIR_RXONLY)
                    .len(chunk_len.saturating_sub(1) as u32)
            });

            // Prevent out-of-bounds panics on split_at_mut.
            if chunk_len > dest.len() {
                return Err(SpiError::InvalidTransaction);
            }
            // Split dest into the chunk we want to read now, and the rest for later.
            let (mut chunk_dest, rest) = dest.split_at_mut(chunk_len);
            dest = rest;

            let mut rx_timeout = TIMEOUT_LIMIT;
            while !chunk_dest.is_empty() {
                let status = regs.status().read();
                let rxqd = status.rxqd() as usize; // rxqd is in 32-bit words
                let bytes_in_fifo = rxqd.checked_mul(4).ok_or(SpiError::HardwareError)?;

                if bytes_in_fifo > 0 {
                    let drain_len = core::cmp::min(bytes_in_fifo, chunk_dest.len());
                    let (drain_chunk, remaining) = chunk_dest.split_at_mut(drain_len);
                    util_regcpy::copy_from_reg_unaligned(drain_chunk, &regs.rxdata());
                    chunk_dest = remaining;
                    rx_timeout = TIMEOUT_LIMIT; // reset timeout when progress is made
                } else {
                    rx_timeout = rx_timeout.checked_sub(1).ok_or(SpiError::Timeout)?;
                }
            }
        }
        Ok(())
    }
}

const TIMEOUT_LIMIT: usize = 10_000_000;
const MAX_RX_CHUNK_LEN: usize = 256;
const MAX_TX_CHUNK_LEN: usize = 288;
const DIR_RXONLY: u32 = 1;
const DIR_TXONLY: u32 = 2;

impl<Mmio: SpiHostMmio> embedded_hal::spi::ErrorType for SpiHost<Mmio> {
    type Error = SpiError;
}

impl<Mmio: SpiHostMmio> embedded_hal::spi::SpiDevice for SpiHost<Mmio> {
    fn transaction(
        &mut self,
        operations: &mut [embedded_hal::spi::Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        let op_count = operations.len();
        for (i, op) in operations.iter_mut().enumerate() {
            let is_last = i == op_count - 1;
            let csaat = !is_last;

            match op {
                embedded_hal::spi::Operation::Read(buf) => {
                    self.read_cmd(buf.len(), 0, csaat, buf)?;
                }
                embedded_hal::spi::Operation::Write(buf) => {
                    self.write_cmd(buf, 0, csaat)?;
                }
                embedded_hal::spi::Operation::Transfer(_, _)
                | embedded_hal::spi::Operation::TransferInPlace(_) => {
                    // Full-duplex SPI transfer is not supported by this simple driver yet.
                    return Err(SpiError::InvalidTransaction);
                }
                embedded_hal::spi::Operation::DelayNs(_) => {
                    // Delay operation is not supported yet.
                    return Err(SpiError::InvalidTransaction);
                }
            }
        }

        // Wait for the controller to finish the last command segment.
        let mut timeout = TIMEOUT_LIMIT;
        while self.is_active() {
            timeout = timeout.checked_sub(1).ok_or(SpiError::Timeout)?;
        }
        // Disable output buffers to release the bus.
        self.mmio
            .regs_mut()
            .control()
            .modify(|w| w.output_en(false));

        Ok(())
    }
}
