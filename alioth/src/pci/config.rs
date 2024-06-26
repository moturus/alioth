// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::mem::size_of;
use std::sync::Arc;

use bitflags::bitflags;
use macros::Layout;
use parking_lot::RwLock;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::mem::emulated::Mmio;
use crate::pci::cap::PciCapList;
use crate::pci::{Bdf, PciBar};
use crate::{assign_bits, mask_bits, mem, unsafe_impl_zerocopy};

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Command: u16 {
        const INTX_DISABLE = 1 << 10;
        const SERR = 1 << 8;
        const PARITY_ERR = 1 << 6;
        const BUS_MASTER = 1 << 2;
        const MEM = 1 << 1;
        const IO = 1 << 0;
        const WRITABLE_BITS = Self::INTX_DISABLE.bits()
            | Self::SERR.bits()
            | Self::PARITY_ERR.bits()
            | Self::BUS_MASTER.bits()
            | Self::MEM.bits()
            | Self::IO.bits();
    }
}
unsafe_impl_zerocopy!(Command, FromBytes, FromZeroes, AsBytes);

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Status: u16 {
        const PARITY_ERR = 1 << 15;
        const SYSTEM_ERR = 1 << 14;
        const RECEIVED_MASTER_ABORT = 1 << 13;
        const RECEIVED_TARGET_ABORT = 1 << 12;
        const SIGNALED_TARGET_ABORT = 1 << 11;
        const MASTER_PARITY_ERR = 1 << 8;
        const CAP = 1 << 4;
        const INTX = 1 << 3;
        const IMMEDIATE_READINESS = 1 << 0;
        const RW1C_BITS = Self::PARITY_ERR.bits()
            | Self::SYSTEM_ERR.bits()
            | Self::RECEIVED_MASTER_ABORT.bits()
            | Self::RECEIVED_TARGET_ABORT.bits()
            | Self::SIGNALED_TARGET_ABORT.bits()
            | Self::MASTER_PARITY_ERR.bits();
    }
}
unsafe_impl_zerocopy!(Status, FromBytes, FromZeroes, AsBytes);

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum HeaderType {
    Device = 0,
    Bridge = 1,
}

#[derive(Debug, Clone, Default, FromBytes, FromZeroes, AsBytes, Layout)]
#[repr(C, align(8))]
pub struct CommonHeader {
    pub vendor: u16,
    pub device: u16,
    pub command: Command,
    pub status: Status,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    pub cache_line_size: u8,
    pub latency_timer: u8,
    pub header_type: u8,
    pub bist: u8,
}

#[derive(Debug, Clone, Default, FromBytes, FromZeroes, AsBytes, Layout)]
#[repr(C, align(8))]
pub struct DeviceHeader {
    pub common: CommonHeader,
    pub bars: [u32; 6],
    pub cardbus_cis_pointer: u32,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
    pub expansion_rom: u32,
    pub capability_pointer: u8,
    pub reserved: [u8; 7],
    pub intx_line: u8,
    pub intx_pin: u8,
    pub min_gnt: u8,
    pub max_lat: u8,
}

pub const OFFSET_BAR0: usize = DeviceHeader::OFFSET_BARS;
pub const OFFSET_BAR5: usize = OFFSET_BAR0 + 5 * size_of::<u32>();

pub const BAR_PREFETCHABLE: u32 = 0b1000;
pub const BAR_MEM64: u32 = 0b0100;
pub const BAR_MEM32: u32 = 0b0000;
pub const BAR_IO: u32 = 0b01;

#[derive(Debug)]
pub enum ConfigHeader {
    Device(DeviceHeader),
}

#[derive(Debug)]
pub struct HeaderData {
    pub header: ConfigHeader,
    pub bar_masks: [u32; 6],
    pub bdf: Bdf,
}

impl HeaderData {
    pub fn set_bar(&mut self, index: usize, val: u32) -> (u32, u32) {
        match &mut self.header {
            ConfigHeader::Device(header) => {
                let mask = self.bar_masks[index];
                let old_val = header.bars[index];
                let masked_val = mask_bits!(old_val, val, mask);
                header.bars[index] = masked_val;
                log::info!(
                    "{}: bar {index}: set to {val:#010x}, update: {old_val:#010x} -> {masked_val:#010x}",
                    self.bdf
                );
                (old_val, masked_val)
            }
        }
    }

    pub fn set_command(&mut self, command: Command) {
        match &mut self.header {
            ConfigHeader::Device(header) => header.common.command = command,
        }
    }

    fn write_header(&mut self, offset: usize, size: u8, val: u64) {
        match &mut self.header {
            ConfigHeader::Device(header) => match (offset, size as usize) {
                CommonHeader::LAYOUT_COMMAND => {
                    let val = Command::from_bits_retain(val as u16);
                    let old = header.common.command;
                    assign_bits!(header.common.command, val, Command::WRITABLE_BITS);
                    let current = header.common.command;
                    log::trace!(
                        "{}: write command: {val:x?}\n   {old:x?}\n-> {current:x?}",
                        self.bdf
                    );
                }
                CommonHeader::LAYOUT_STATUS => {
                    let val = Status::from_bits_retain(val as u16);
                    let old = header.common.status;
                    header.common.status &= !(val & Status::RW1C_BITS);
                    log::trace!(
                        "{}: write status: {val:x?}\n   {old:x?}\n-> {:x?}",
                        self.bdf,
                        header.common.status,
                    );
                }
                (OFFSET_BAR0..=OFFSET_BAR5, 4) => {
                    let bar_index = (offset - OFFSET_BAR0) >> 2;

                    let mask = self.bar_masks[bar_index];
                    let old_val = header.bars[bar_index];
                    let masked_val = mask_bits!(old_val, val as u32, mask);

                    log::info!(
                        "{}: updating bar {}: {:x} -> {:x}, mask={:x}",
                        self.bdf,
                        bar_index,
                        old_val,
                        masked_val,
                        mask
                    );
                    header.bars[bar_index] = masked_val;
                }
                _ => {
                    log::warn!(
                        "unaligned write offset = {offset:#x}, size = {size}, val = {val:#x}"
                    );
                }
            },
        }
    }
}

#[derive(Debug)]
pub struct EmulatedHeader {
    pub data: Arc<RwLock<HeaderData>>,
    pub bars: [PciBar; 6],
}

impl EmulatedHeader {
    pub fn set_bdf(&self, bdf: Bdf) {
        self.data.write().bdf = bdf
    }

    pub fn set_command(&self, command: Command) {
        let mut header = self.data.write();
        header.set_command(command)
    }
}

impl Mmio for EmulatedHeader {
    fn size(&self) -> usize {
        0x40
    }

    fn read(&self, offset: usize, size: u8) -> mem::Result<u64> {
        let data = self.data.read();
        let bytes = match &data.header {
            ConfigHeader::Device(header) => AsBytes::as_bytes(header),
        };
        let ret = match size {
            1 => bytes.get(offset).map(|b| *b as u64),
            2 => u16::read_from_prefix(&bytes[offset..]).map(|w| w as u64),
            4 => u32::read_from_prefix(&bytes[offset..]).map(|d| d as u64),
            8 => u64::read_from_prefix(&bytes[offset..]),
            _ => Some(0),
        };
        Ok(ret.unwrap_or(0))
    }

    fn write(&self, offset: usize, size: u8, val: u64) -> mem::Result<()> {
        let mut data = self.data.write();
        data.write_header(offset, size, val);
        Ok(())
    }
}

pub trait PciConfig: Mmio {
    fn get_header(&self) -> &EmulatedHeader;
}

#[derive(Debug)]
pub struct EmulatedConfig {
    pub header: EmulatedHeader,
    pub caps: PciCapList,
}

impl Mmio for EmulatedConfig {
    fn read(&self, offset: usize, size: u8) -> mem::Result<u64> {
        if offset < size_of::<DeviceHeader>() {
            self.header.read(offset, size)
        } else {
            self.caps.read(offset, size)
        }
    }

    fn write(&self, offset: usize, size: u8, val: u64) -> mem::Result<()> {
        if offset < size_of::<DeviceHeader>() {
            self.header.write(offset, size, val)
        } else {
            self.caps.write(offset, size, val)
        }
    }

    fn size(&self) -> usize {
        4096
    }
}

impl EmulatedConfig {
    pub fn new_device(
        mut header: DeviceHeader,
        bar_masks: [u32; 6],
        bars: [PciBar; 6],
        caps: PciCapList,
    ) -> EmulatedConfig {
        if !caps.is_empty() {
            header.common.status |= Status::CAP;
            header.capability_pointer = size_of::<DeviceHeader>() as u8;
        }
        let header = EmulatedHeader {
            data: Arc::new(RwLock::new(HeaderData {
                header: ConfigHeader::Device(header),
                bar_masks,
                bdf: Bdf(0),
            })),
            bars,
        };
        EmulatedConfig { header, caps }
    }
}

impl PciConfig for EmulatedConfig {
    fn get_header(&self) -> &EmulatedHeader {
        &self.header
    }
}
