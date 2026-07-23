// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Flash IPC client implementation.

#![no_std]
use core::num::NonZero;

use hal_flash::{Flash, FlashAddress};
use services_flash_opcode::*;
use userspace::time::Instant;
use util_error::{self as error, ErrorCode};
use util_ipc::{IpcChannel, IpcHandle};
use util_types::PowerOf2Usize;
use zerocopy::{FromZeros, IntoBytes};

/// An IPC-based client for the flash service.
///
/// This struct implements the `Flash` trait by proxying requests to a remote
/// flash server via an IPC handle.
pub struct FlashIpcClient {
    ipc: IpcHandle,
    page_size: PowerOf2Usize,
    total_size: NonZero<usize>,
    erasable_sizes_bitmap: u32,
}

/// An RAII guard providing exclusive access to a `FlashIpcClient`.
/// Automatically releases the lock when dropped.
pub struct FlashLockGuard<'a> {
    client: &'a mut FlashIpcClient,
}

impl Drop for FlashLockGuard<'_> {
    fn drop(&mut self) {
        let _ = self.client.unlock_ipc();
    }
}

impl Flash for FlashLockGuard<'_> {
    type Error = ErrorCode;

    fn geometry(&mut self) -> Result<(NonZero<usize>, PowerOf2Usize, u32), ErrorCode> {
        self.client.geometry()
    }

    fn erase(&mut self, start_addr: FlashAddress, size: PowerOf2Usize) -> Result<(), ErrorCode> {
        self.client.erase(start_addr, size)
    }

    fn program(&mut self, start_addr: FlashAddress, data: &[u8]) -> Result<(), ErrorCode> {
        self.client.program(start_addr, data)
    }

    fn read(&mut self, start_addr: FlashAddress, buf: &mut [u8]) -> Result<(), ErrorCode> {
        self.client.read(start_addr, buf)
    }
}

impl FlashIpcClient {
    /// Creates a new `FlashIpcClient` using the provided IPC handle.
    ///
    /// This constructor will perform an IPC transaction to retrieve flash
    /// geometry and capabilities from the server.
    pub fn new(ipc: IpcHandle) -> Result<Self, ErrorCode> {
        let mut info = FlashInfo::new_zeroed();
        let mut result = 0u32;

        ipc.transact(
            &[IPC_OP_FLASH_GET_INFO.as_bytes()],
            &mut [result.as_mut_bytes(), info.as_mut_bytes()],
            Instant::MAX,
        )
        .map_err(ErrorCode::kernel_error)?;
        ErrorCode::check_status(result)?;

        let Some(page_size) = PowerOf2Usize::new(info.page_size as usize) else {
            return Err(error::FLASH_GENERIC_INVALID_PAGE_SIZE);
        };
        let Some(total_size) = NonZero::new(info.total_size as usize) else {
            return Err(error::FLASH_GENERIC_INVALID_SIZE);
        };
        Ok(Self {
            ipc,
            page_size,
            total_size,
            erasable_sizes_bitmap: info.erasable_sizes_bitmap,
        })
    }

    /// Non-blocking lock attempt (try_lock).
    ///
    /// Fails immediately with `FLASH_GENERIC_LOCKED` if the flash server is locked.
    pub fn try_lock(&mut self) -> Result<FlashLockGuard<'_>, ErrorCode> {
        let mut result = 0u32;
        self.ipc
            .transact(
                &[IPC_OP_FLASH_TRY_LOCK.as_bytes()],
                &mut [result.as_mut_bytes()],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        ErrorCode::check_status(result)?;
        pw_log::info!("FlashIpcClient: try_lock acquired");
        Ok(FlashLockGuard { client: self })
    }

    /// Blocking lock attempt (lock) returning an RAII FlashLockGuard.
    ///
    /// Blocks until the flash lock is acquired.
    pub fn lock(&mut self) -> Result<FlashLockGuard<'_>, ErrorCode> {
        self.lock_ipc()?;
        Ok(FlashLockGuard { client: self })
    }

    /// Explicitly acquire the flash lock over IPC (without returning an RAII guard).
    pub fn lock_ipc(&mut self) -> Result<(), ErrorCode> {
        let mut result = 0u32;
        self.ipc
            .transact(
                &[IPC_OP_FLASH_LOCK.as_bytes()],
                &mut [result.as_mut_bytes()],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        let status = ErrorCode::check_status(result);
        if status.is_ok() {
            pw_log::info!("FlashIpcClient: lock acquired");
        }
        status
    }

    /// Explicitly release the flash lock over IPC.
    pub fn unlock_ipc(&mut self) -> Result<(), ErrorCode> {
        let mut result = 0u32;
        self.ipc
            .transact(
                &[IPC_OP_FLASH_UNLOCK.as_bytes()],
                &mut [result.as_mut_bytes()],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        let status = ErrorCode::check_status(result);
        if status.is_ok() {
            pw_log::info!("FlashIpcClient: unlock released");
        }
        status
    }
}

impl Flash for FlashIpcClient {
    type Error = ErrorCode;
    fn geometry(&mut self) -> Result<(NonZero<usize>, PowerOf2Usize, u32), ErrorCode> {
        Ok((self.total_size, self.page_size, self.erasable_sizes_bitmap))
    }

    fn erase(&mut self, start_addr: FlashAddress, size: PowerOf2Usize) -> Result<(), ErrorCode> {
        let mut result = 0u32;
        let op = EraseOp {
            address: start_addr,
            size: size.get() as u32,
        };
        self.ipc
            .transact(
                &[IPC_OP_FLASH_ERASE.as_bytes(), op.as_bytes()],
                &mut [result.as_mut_bytes()],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        ErrorCode::check_status(result)
    }

    fn program(&mut self, start_addr: FlashAddress, data: &[u8]) -> Result<(), ErrorCode> {
        let mut result = 0u32;
        self.ipc
            .transact(
                &[IPC_OP_FLASH_PROGRAM.as_bytes(), start_addr.as_bytes(), data],
                &mut [result.as_mut_bytes()],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        ErrorCode::check_status(result)
    }

    fn read(&mut self, start_addr: FlashAddress, buf: &mut [u8]) -> Result<(), ErrorCode> {
        let mut result = 0u32;
        let op = ReadOp {
            address: start_addr,
            length: buf.len() as u32,
        };
        self.ipc
            .transact(
                &[IPC_OP_FLASH_READ.as_bytes(), op.as_bytes()],
                &mut [result.as_mut_bytes(), buf],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        ErrorCode::check_status(result)
    }
}
