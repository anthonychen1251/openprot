// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Flash IPC server implementation.

#![no_std]

use hal_flash::{Flash, FlashAddress};
use services_flash_opcode::*;
use util_error::{self as error, ErrorCode};
use util_ipc::{IpcChannel, IpcHandle};
use util_types::{Opcode, PowerOf2Usize};
use zerocopy::{FromBytes, IntoBytes};

/// An IPC server wrapper that accepts a flash implementation and exposes
/// an IPC interface to it.
pub struct FlashIpcServer<TFlash: Flash> {
    flash: TFlash,
    locked_by: Option<usize>,
    name: &'static str,
}

impl<TFlash: Flash<Error = ErrorCode>> FlashIpcServer<TFlash> {
    /// Creates a new `FlashIpcServer` wrapping the given flash implementation.
    pub fn new(flash: TFlash) -> Self {
        Self::new_with_name(flash, "flash")
    }

    /// Creates a new `FlashIpcServer` with a specific name for logging.
    pub fn new_with_name(flash: TFlash, name: &'static str) -> Self {
        Self {
            flash,
            locked_by: None,
            name,
        }
    }

    /// Returns `true` if the server is currently locked by any client.
    pub fn is_locked(&self) -> bool {
        self.locked_by.is_some()
    }

    /// Returns `true` if the server is locked by the specified token.
    pub fn is_locked_by(&self, token: usize) -> bool {
        self.locked_by == Some(token)
    }

    /// Forces setting the lock owner token directly (used when granting a pending lock).
    pub fn force_lock(&mut self, token: usize) {
        pw_log::info!(
            "FlashIpcServer({}): locked by token {}",
            self.name as &str,
            token as u32
        );
        self.locked_by = Some(token);
    }

    fn handle_try_lock<'a>(&mut self, token: usize) -> Result<&'a [u8], ErrorCode> {
        match self.locked_by {
            None => {
                pw_log::info!(
                    "FlashIpcServer({}): locked by token {}",
                    self.name as &str,
                    token as u32
                );
                self.locked_by = Some(token);
                Ok(&[])
            }
            Some(owner) => {
                pw_log::warn!(
                    "FlashIpcServer({}): lock request by token {} denied; already locked by token {}",
                    self.name as &str,
                    token as u32,
                    owner as u32
                );
                Err(error::FLASH_GENERIC_LOCKED)
            }
        }
    }

    fn handle_unlock<'a>(&mut self, token: usize) -> Result<&'a [u8], ErrorCode> {
        if self.locked_by == Some(token) {
            pw_log::info!(
                "FlashIpcServer({}): unlocked by token {}",
                self.name as &str,
                token as u32
            );
            self.locked_by = None;
            Ok(&[])
        } else {
            if let Some(owner) = self.locked_by {
                pw_log::warn!(
                    "FlashIpcServer({}): unlock request by token {} denied; currently locked by token {}",
                    self.name as &str,
                    token as u32,
                    owner as u32
                );
            } else {
                pw_log::warn!(
                    "FlashIpcServer({}): unlock request by token {} denied; server is not locked",
                    self.name as &str,
                    token as u32
                );
            }
            Err(error::FLASH_GENERIC_NOT_LOCKED)
        }
    }

    /// Handles the `IPC_OP_FLASH_GET_INFO` request.
    ///
    /// Writes the flash geometry into the provided buffer and returns it.
    fn handle_geometry<'a>(
        &mut self,
        data: &'a mut [u8],
        reqsz: usize,
    ) -> Result<&'a [u8], ErrorCode> {
        if reqsz != 0 {
            return Err(error::IPC_ERROR_BAD_REQ_LEN);
        }
        let (info, _rest) =
            FlashInfo::mut_from_prefix(data).map_err(|_| error::IPC_ERROR_BAD_REQ_LEN)?;
        let (total_size, page_size, erasable_sizes_bitmap) = self.flash.geometry()?;
        info.page_size = page_size.get() as u32;
        info.total_size = total_size.get() as u32;
        info.erasable_sizes_bitmap = erasable_sizes_bitmap;
        Ok(info.as_bytes())
    }

    /// Handles the `IPC_OP_FLASH_ERASE` request.
    ///
    /// Parses the `EraseOp` from the input data and erases the specified block.
    fn handle_erase<'a>(
        &mut self,
        data: &'a mut [u8],
        reqsz: usize,
    ) -> Result<&'a [u8], ErrorCode> {
        let req_data = data.get(..reqsz).ok_or(error::IPC_ERROR_BAD_REQ_LEN)?;
        let op = EraseOp::read_from_bytes(req_data).map_err(|_| error::IPC_ERROR_BAD_REQ_LEN)?;
        let Some(size) = PowerOf2Usize::new(op.size as usize) else {
            return Err(error::FLASH_GENERIC_ERASE_INVALID_SIZE);
        };
        self.flash.erase(op.address, size)?;
        Ok(&data[0..0])
    }

    /// Handles the `IPC_OP_FLASH_PROGRAM` request.
    ///
    /// Parses the start address and data from the input, then programs it.
    fn handle_program<'a>(
        &mut self,
        data: &'a mut [u8],
        reqsz: usize,
    ) -> Result<&'a [u8], ErrorCode> {
        let req_data = data.get(..reqsz).ok_or(error::IPC_ERROR_BAD_REQ_LEN)?;
        let (addr, program_data) =
            FlashAddress::read_from_prefix(req_data).map_err(|_| error::IPC_ERROR_BAD_REQ_LEN)?;
        self.flash.program(addr, program_data)?;
        Ok(&data[0..0])
    }

    /// Handles the `IPC_OP_FLASH_READ` request.
    ///
    /// Parses the `ReadOp` from the input, reads the data from flash into the
    /// buffer, and returns the read slice.
    fn handle_read<'a>(&mut self, data: &'a mut [u8], reqsz: usize) -> Result<&'a [u8], ErrorCode> {
        let req_data = data.get(..reqsz).ok_or(error::IPC_ERROR_BAD_REQ_LEN)?;
        let op = ReadOp::read_from_bytes(req_data).map_err(|_| error::IPC_ERROR_BAD_REQ_LEN)?;
        let length = op.length as usize;
        if length > data.len() {
            return Err(error::FLASH_GENERIC_INVALID_SIZE);
        }
        self.flash.read(op.address, &mut data[..length])?;
        Ok(&data[..length])
    }

    /// Handles an IPC operation for a given client token.
    fn handle_op<'a>(
        &mut self,
        token: usize,
        opcode: Opcode,
        data: &'a mut [u8],
        reqsz: usize,
    ) -> Result<&'a [u8], ErrorCode> {
        if let Some(owner) = self.locked_by {
            if owner != token && opcode != IPC_OP_FLASH_TRY_LOCK && opcode != IPC_OP_FLASH_LOCK {
                return Err(error::FLASH_GENERIC_LOCKED);
            }
        }

        match opcode {
            IPC_OP_FLASH_TRY_LOCK | IPC_OP_FLASH_LOCK => self.handle_try_lock(token),
            IPC_OP_FLASH_UNLOCK => self.handle_unlock(token),
            IPC_OP_FLASH_GET_INFO => self.handle_geometry(data, reqsz),
            IPC_OP_FLASH_ERASE => self.handle_erase(data, reqsz),
            IPC_OP_FLASH_PROGRAM => self.handle_program(data, reqsz),
            IPC_OP_FLASH_READ => self.handle_read(data, reqsz),
            _ => Err(error::IPC_ERROR_UNKNOWN_OP),
        }
    }

    /// Handles a single IPC request using a default token (0).
    pub fn handle_one(&mut self, ipc: &IpcHandle, data: &mut [u8]) -> Result<(), ErrorCode> {
        self.handle_one_with_token(0, ipc, data).map(|_| ())
    }

    /// Handles a single IPC request with a specific client token.
    ///
    /// This method performs a non-blocking read on the IPC handle. The caller
    /// must ensure the handle is readable (e.g., by calling `syscall::object_wait`)
    /// before calling this method.
    ///
    /// Returns `Ok((opcode, status_result))` on processing the request. If the request
    /// was a blocking `IPC_OP_FLASH_LOCK` attempt that failed because the server is locked,
    /// `status_result` will be `Err(FLASH_GENERIC_LOCKED)` and `ipc.respond` is DEFERRED
    /// (not called), allowing the caller (`FlashService`) to suspend the client.
    pub fn handle_one_with_token(
        &mut self,
        token: usize,
        ipc: &IpcHandle,
        data: &mut [u8],
    ) -> Result<(Opcode, Result<(), ErrorCode>), ErrorCode> {
        let len = ipc.read(0, data).map_err(ErrorCode::kernel_error)?;
        let (opcode_bytes, reqrsp) = data.split_at_mut(core::mem::size_of::<Opcode>());
        let opcode =
            Opcode::read_from_bytes(opcode_bytes).map_err(|_| error::IPC_ERROR_BAD_REQ_LEN)?;
        let len = len.saturating_sub(core::mem::size_of::<Opcode>());

        let res = self.handle_op(token, opcode, reqrsp, len);
        if opcode == IPC_OP_FLASH_LOCK && res == Err(error::FLASH_GENERIC_LOCKED) {
            // Defer response so client blocks in microkernel
            return Ok((opcode, Err(error::FLASH_GENERIC_LOCKED)));
        }

        let mut status = 0u32;
        let result = match res {
            Ok(result) => result,
            Err(e) => {
                status = e.0.get();
                &[]
            }
        };
        ipc.respond(&[status.as_bytes(), result])
            .map_err(ErrorCode::kernel_error)?;
        Ok((opcode, res.map(|_| ())))
    }
}
