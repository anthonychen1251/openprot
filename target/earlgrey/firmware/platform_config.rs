// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_std]

pub use earlgrey_gpio::{EarlGreyGpio, EarlGreyPinConfig, GpioMask, GpioPin};
pub use earlgrey_pinmux::{Pad, PadConfig, Pull};
pub use openprot_hal_blocking::gpio_port::{
    EdgeSensitivity, GpioInterrupt, GpioPort, InterruptOperation, PinMask,
};

pub const USB_PRESENCE_GPIO: GpioPin = GpioPin::Pin3;
pub const USB_MUX_CTRL_GPIO: GpioPin = GpioPin::Pin4;

/// A wrapper around `Pad` that does not implement `Copy` or `Clone`.
/// This allows ensuring that each pad is used only once in a configuration by treating assignments as moves.
pub struct UsedPad(pub Pad);

#[derive(Debug, Clone, Copy)]
pub struct ConfiguredPad {
    pub pad: Pad,
    pub config: PadConfig,
}

impl ConfiguredPad {
    pub fn with_pull(mut self, pull: Pull) -> Self {
        self.config.pull = pull;
        self
    }

    pub fn with_open_drain(mut self) -> Self {
        self.config.open_drain = true;
        self
    }

    pub fn with_inverted(mut self) -> Self {
        self.config.invert = true;
        self
    }
}

impl UsedPad {
    pub fn with_default(self) -> ConfiguredPad {
        ConfiguredPad {
            pad: self.0,
            config: PadConfig::default(),
        }
    }

    pub fn with_pull(self, pull: Pull) -> ConfiguredPad {
        self.with_default().with_pull(pull)
    }

    pub fn with_open_drain(self) -> ConfiguredPad {
        self.with_default().with_open_drain()
    }

    pub fn with_inverted(self) -> ConfiguredPad {
        self.with_default().with_inverted()
    }
}

macro_rules! define_pad_pool {
    ($($variant:ident),*) => {
        #[allow(non_snake_case)]
        pub struct PadPool {
            $( pub $variant: UsedPad, )*
        }
        impl PadPool {
            pub const fn new() -> Self {
                Self { $( $variant: UsedPad(Pad::$variant), )* }
            }
        }
        impl Default for PadPool {
            fn default() -> Self {
                Self::new()
            }
        }
    }
}

define_pad_pool!(
    IOA0, IOA1, IOA2, IOA3, IOA4, IOA5, IOA6, IOA7, IOA8, IOB0, IOB1, IOB2, IOB3, IOB4, IOB5, IOB6,
    IOB7, IOB8, IOB9, IOB10, IOB11, IOB12, IOC0, IOC1, IOC2, IOC3, IOC4, IOC5, IOC6, IOC7, IOC8,
    IOC9, IOC10, IOC11, IOC12, IOR0, IOR1, IOR2, IOR3, IOR4, IOR5, IOR6, IOR7, IOR10, IOR11, IOR12,
    IOR13
);

#[derive(Debug, Clone, Copy)]
pub struct UsbMuxCtrl {
    pub pad: ConfiguredPad,
    pub controlled_by_presence: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct UsbPresenceConfig {
    pub presence_detect: ConfiguredPad,
    pub mux_ctrl: Option<UsbMuxCtrl>,
}

#[derive(Debug, Clone, Copy)]
pub struct PlatformConfig {
    pub usb_presence: Option<UsbPresenceConfig>,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        let p = PadPool::new();
        Self {
            usb_presence: Some(UsbPresenceConfig {
                presence_detect: p.IOR11.with_pull(Pull::Up),
                mux_ctrl: Some(UsbMuxCtrl {
                    pad: p.IOC6.with_default(),
                    controlled_by_presence: false,
                }),
            }),
        }
    }
}

pub trait PinmuxConfig {
    fn pinmux_config(&self, gpio: &mut EarlGreyGpio);
}

impl PinmuxConfig for UsbPresenceConfig {
    fn pinmux_config(&self, gpio: &mut EarlGreyGpio) {
        // Presence detect
        let presence_cfg = EarlGreyPinConfig {
            is_input: true,
            is_output: false,
            input_filter: false,
            pad: Some(self.presence_detect.pad),
            pull: self.presence_detect.config.pull,
        };
        let _ = gpio.configure(USB_PRESENCE_GPIO.into(), presence_cfg);
        let _ = gpio.irq_configure(USB_PRESENCE_GPIO.into(), EdgeSensitivity::BothEdges);
        let _ = gpio.irq_control(USB_PRESENCE_GPIO.into(), InterruptOperation::Enable);

        // Mux ctrl
        if let Some(mux_ctrl) = &self.mux_ctrl {
            let _ = gpio.set_reset(GpioMask::empty(), USB_MUX_CTRL_GPIO.into());
            let mux_cfg = EarlGreyPinConfig {
                is_input: false,
                is_output: true,
                input_filter: false,
                pad: Some(mux_ctrl.pad.pad),
                pull: mux_ctrl.pad.config.pull,
            };
            let _ = gpio.configure(USB_MUX_CTRL_GPIO.into(), mux_cfg);
        }
    }
}

impl PinmuxConfig for PlatformConfig {
    fn pinmux_config(&self, gpio: &mut EarlGreyGpio) {
        if let Some(usb_presence) = &self.usb_presence {
            usb_presence.pinmux_config(gpio);
        }
    }
}

/// Returns true if USB presence is detected (active Low on USB_PRESENCE_GPIO).
pub fn check_usb_front_panel_presence(gpio: &EarlGreyGpio) -> bool {
    if let Ok(input) = gpio.read_input() {
        let mask: GpioMask = USB_PRESENCE_GPIO.into();
        !input.contains(mask)
    } else {
        false
    }
}
