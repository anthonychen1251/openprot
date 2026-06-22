// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Manifest parsing utilities for Earlgrey firmware images.

use crate::tags::ManifestIdentifier;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C)]
pub struct Manifest {
    pub signature: [u8; 64],
    pub reserved_signature: [u8; 160],
    pub reserved_unsigned: [u8; 160],
    pub usage_constraints: [u8; 48],
    pub pub_key: [u8; 64],
    pub reserved_public_key: [u8; 160],
    pub reserved: [u8; 156],
    pub manifest_base_address: u32,
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
            signature: [0u8; 64],
            reserved_signature: [0u8; 160],
            reserved_unsigned: [0u8; 160],
            usage_constraints: [0u8; 48],
            pub_key: [0u8; 64],
            reserved_public_key: [0u8; 160],
            reserved: [0u8; 156],
            manifest_base_address: 0,
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
