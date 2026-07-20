// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]

use earlgrey_platform_client::op;
use platform_config::{
    check_usb_front_panel_presence, EarlGreyGpio, GpioInterrupt, InterruptOperation,
    USB_PRESENCE_GPIO,
};
use util_error::{self as error, ErrorCode};
use util_ipc::IpcChannel;
use util_types::Opcode;
use zerocopy::{FromBytes, IntoBytes};
use zfmt::Zfmt;

#[derive(Zfmt)]
#[zfmt(format = "USB front panel state: {status}")]
struct UsbFrontPanelStatus {
    status: &'static str,
}

pub struct PlatformServer {
    gpio: EarlGreyGpio,
    has_pending_usb_wait: bool,
}

impl PlatformServer {
    pub fn new(gpio: EarlGreyGpio) -> Self {
        let is_present = check_usb_front_panel_presence(&gpio);
        let status = if is_present {
            "CONNECTED"
        } else {
            "DISCONNECTED"
        };
        util_zfmt::info!(UsbFrontPanelStatus { status });
        Self {
            gpio,
            has_pending_usb_wait: false,
        }
    }

    pub fn handle_gpio_interrupt(&mut self) -> Result<bool, ErrorCode> {
        let _ = self
            .gpio
            .irq_control(USB_PRESENCE_GPIO.into(), InterruptOperation::Clear);

        let is_present = check_usb_front_panel_presence(&self.gpio);
        let status = if is_present {
            "CONNECTED"
        } else {
            "DISCONNECTED"
        };
        util_zfmt::info!(UsbFrontPanelStatus { status });

        if self.has_pending_usb_wait && !is_present {
            self.has_pending_usb_wait = false;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn handle_wait_usb_disconnected<'a>(
        &mut self,
        data: &'a mut [u8],
        _reqsz: usize,
    ) -> Result<Option<&'a [u8]>, ErrorCode> {
        if !check_usb_front_panel_presence(&self.gpio) {
            Ok(Some(&data[0..0]))
        } else {
            self.has_pending_usb_wait = true;
            Ok(None)
        }
    }

    fn handle_op<'a>(
        &mut self,
        opcode: Opcode,
        data: &'a mut [u8],
        reqsz: usize,
    ) -> Result<Option<&'a [u8]>, ErrorCode> {
        match opcode {
            op::PLATFORM_OP_WAIT_USB_DISCONNECTED => self.handle_wait_usb_disconnected(data, reqsz),
            _ => Err(error::IPC_ERROR_UNKNOWN_OP),
        }
    }

    pub fn handle_one(&mut self, ipc: &impl IpcChannel, data: &mut [u8]) -> Result<(), ErrorCode> {
        let len = ipc.read(0, data).map_err(ErrorCode::kernel_error)?;
        let (opcode, reqrsp) = data.split_at_mut(core::mem::size_of::<Opcode>());
        let opcode = Opcode::read_from_bytes(opcode).map_err(|_| error::IPC_ERROR_BAD_REQ_LEN)?;
        let len = len.saturating_sub(core::mem::size_of::<Opcode>());
        let mut status = 0u32;
        match self.handle_op(opcode, reqrsp, len) {
            Ok(Some(result)) => {
                ipc.respond(&[status.as_bytes(), result])
                    .map_err(ErrorCode::kernel_error)?;
            }
            Ok(None) => {}
            Err(e) => {
                status = e.0.get();
                ipc.respond(&[status.as_bytes(), &[]])
                    .map_err(ErrorCode::kernel_error)?;
            }
        }
        Ok(())
    }

    pub fn respond_pending_usb_wait(&mut self, ipc: &impl IpcChannel) -> Result<(), ErrorCode> {
        let status = 0u32;
        ipc.respond(&[status.as_bytes(), &[]])
            .map_err(ErrorCode::kernel_error)?;
        Ok(())
    }
}
