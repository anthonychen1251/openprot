// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! SPI Host driver for OpenTitan Earlgrey.
//!
//! This driver manages the MMIO registers of the SPI Host controller to perform
//! half-duplex SPI transactions (standard, dual, or quad speed).

#![cfg_attr(not(test), no_std)]

use aligned::{Aligned, A4};
use core::cell::Cell;
use core::cmp::min;

use spi_host::RegisterBlock;
use util_error::ErrorCode;
use util_regcpy::{copy_from_reg, copy_from_reg_unaligned, copy_to_reg_unaligned};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiTxnWidth {
    STANDARD = 0,
    DUAL = 1,
    QUAD = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct SpiOpts {
    pub width: SpiTxnWidth,
    pub hold_cs: bool,
}

const DIR_RXONLY: u32 = 1;
const DIR_TXONLY: u32 = 2;

// The internal FIFO size is 256 bytes; increasing this value beyond that
// has been observed to cause corruption (duplicate byte reads) when the
// peripheral halts and resumes due to the FIFO being full (hw bug?).
pub const MAX_RX_CHUNK_LEN: usize = 256;

// The internal FIFO size is 288 bytes; increasing this value beyond that
// has been observed to cause corruption (spurious clk cycles) when the
// peripheral halts and resumes due to the FIFO being empty (hw bug?).
pub const MAX_TX_CHUNK_LEN: usize = 288;

pub struct SpiHost {
    regs: RegisterBlock<ureg::RealMmioMut<'static>>,
    hold_cs: Cell<bool>,
}

fn speed_from_opts(opts: &SpiOpts) -> u32 {
    match opts.width {
        SpiTxnWidth::STANDARD => 0,
        SpiTxnWidth::DUAL => 1,
        SpiTxnWidth::QUAD => 2,
    }
}

impl SpiHost {
    /// Creates a new `SpiHost` driver instance.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` points to a valid SPI Host peripheral register block
    /// and that they have exclusive access to it.
    pub unsafe fn new(ptr: *mut u32) -> Self {
        // SAFETY: The caller guarantees that `ptr` is valid and exclusive.
        let regs = unsafe { RegisterBlock::new(ptr) };

        // Configure SPI Host controller parameters.
        // SPI0: 1 / (2 * (clk_div + 1) * (1 / 96e6)) == 24MHz.
        // We set clkdiv to 1 for a 24MHz clock frequency.
        regs.configopts().write(|w| {
            w.clkdiv(1)
                .cpha(false)
                .cpol(false)
                .fullcyc(true)
                .csnlead(0)
                .csnidle(2)
                .csntrail(1)
        });

        // Trigger a software reset to clear internal state.
        regs.control().write(|w| w.sw_rst(true));
        let mut i = 0;
        loop {
            if i > 1_000_000 {
                // Prevent infinite loop if hardware is unresponsive.
                break;
            }
            let status = regs.status().read();
            if status.txempty() && status.rxempty() {
                break;
            }
            i += 1;
        }

        // Enable SPI Host.
        regs.control().write(|w| w.sw_rst(false).spien(true));
        regs.csid().write(|_| 0);

        Self {
            regs,
            hold_cs: Cell::new(false),
        }
    }

    /// Writes data to the SPI bus.
    pub fn write(&self, mut req: &[u8], opts: &SpiOpts) -> Result<(), ErrorCode> {
        self.regs.control().modify(|w| w.output_en(true));

        while !req.is_empty() {
            while !self.is_ready() {}

            let cmd_chunk = &req[..min(req.len(), MAX_TX_CHUNK_LEN)];
            req = &req[cmd_chunk.len()..];

            copy_to_reg_unaligned(&self.regs.txdata(), cmd_chunk);

            self.regs.command().write(|w| {
                w.speed(speed_from_opts(opts))
                    .csaat(!req.is_empty() || opts.hold_cs)
                    .direction(DIR_TXONLY)
                    .len(cmd_chunk.len() as u32 - 1)
            });
        }

        self.hold_cs.set(opts.hold_cs);

        if !opts.hold_cs {
            // Flush the transmit FIFO.
            while self.is_active() {}
            self.regs.control().modify(|w| w.output_en(false));
        }

        Ok(())
    }

    /// Reads data from the SPI bus.
    pub fn read(&self, mut resp: &mut [u8], opts: &SpiOpts) -> Result<(), ErrorCode> {
        self.regs.control().modify(|w| w.output_en(true));

        while !resp.is_empty() {
            while !self.is_ready() {}

            let resp_len = resp.len();
            let mut cmd_chunk = &mut resp[..min(resp_len, MAX_RX_CHUNK_LEN)];
            let cmd_chunk_len = cmd_chunk.len();

            self.regs.command().write(|w| {
                w.speed(speed_from_opts(opts))
                    .csaat(opts.hold_cs || resp_len != cmd_chunk_len)
                    .direction(DIR_RXONLY)
                    .len(cmd_chunk_len as u32 - 1)
            });

            while !cmd_chunk.is_empty() {
                let fifo_chunk = self.drain_fifo(cmd_chunk)?;
                let chunk_len = fifo_chunk.len();
                cmd_chunk = &mut cmd_chunk[chunk_len..];
            }
            resp = &mut resp[cmd_chunk_len..];
        }

        Ok(())
    }

    /// Triggers a speculative read (prefetch) command.
    pub fn prefetch(&self, opts: &SpiOpts, len: usize) -> Result<(), ErrorCode> {
        self.regs.control().modify(|w| w.output_en(true));
        while !self.is_ready() {}

        self.regs.command().write(|w| {
            w.speed(speed_from_opts(opts))
                .csaat(opts.hold_cs)
                .direction(DIR_RXONLY)
                .len((len as u32) - 1)
        });

        self.hold_cs.set(opts.hold_cs);
        Ok(())
    }

    /// Drains the receive FIFO into a non-aligned buffer.
    pub fn drain_fifo<'a>(&self, resp: &'a mut [u8]) -> Result<&'a mut [u8], ErrorCode> {
        let status = self.regs.status().read();
        let rx_queue_depth = usize::try_from(status.rxqd()).unwrap();
        let len = min(rx_queue_depth * 4, resp.len());
        let resp = &mut resp[..len];
        copy_from_reg_unaligned(resp, &self.regs.rxdata());

        if !status.active() && !self.hold_cs.get() {
            self.regs.control().modify(|w| w.output_en(false));
        }

        Ok(resp)
    }

    /// Drains the receive FIFO into a 4-byte aligned buffer.
    pub fn aligned_drain_fifo<'a>(
        &self,
        resp: &'a mut Aligned<A4, [u8]>,
    ) -> Result<&'a mut Aligned<A4, [u8]>, ErrorCode> {
        let status = self.regs.status().read();
        let rx_queue_depth = usize::try_from(status.rxqd()).unwrap();
        let len = min(rx_queue_depth * 4, resp.len());
        let resp = &mut resp[..len];
        copy_from_reg::<MAX_RX_CHUNK_LEN>(resp, &self.regs.rxdata());
        Ok(resp)
    }

    /// Returns whether the SPI controller is currently active.
    pub fn is_active(&self) -> bool {
        self.regs.status().read().active()
    }

    /// Returns whether the SPI controller is ready for a new command segment.
    pub fn is_ready(&self) -> bool {
        let status = self.regs.status().read();
        // We must wait until the peripheral is no longer active to avoid FSM
        // synchronization issues when accepting a new command segment.
        status.ready() && !status.active()
    }
}
