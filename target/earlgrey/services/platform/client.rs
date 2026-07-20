// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]

use userspace::time::Instant;
use util_error::ErrorCode;
use util_ipc::IpcChannel;
use zerocopy::IntoBytes;

pub mod op {
    use util_types::Opcode;
    pub const PLATFORM_OP_WAIT_USB_DISCONNECTED: Opcode = Opcode::new(*b"PLUD");
}

pub struct PlatformClient<IPC: IpcChannel> {
    ipc: IPC,
}

impl<IPC: IpcChannel> PlatformClient<IPC> {
    pub const fn new(ipc: IPC) -> Self {
        Self { ipc }
    }

    pub fn wait_until_usb_disconnected(&self) -> Result<(), ErrorCode> {
        let mut result = 0u32;
        self.ipc
            .transact(
                &[op::PLATFORM_OP_WAIT_USB_DISCONNECTED.as_bytes()],
                &mut [result.as_mut_bytes()],
                Instant::MAX,
            )
            .map_err(ErrorCode::kernel_error)?;
        ErrorCode::check_status(result)
    }
}
