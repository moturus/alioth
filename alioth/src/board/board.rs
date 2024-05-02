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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use parking_lot::RwLock;
use thiserror::Error;

use crate::acpi::create_acpi_tables;
use crate::arch::layout::{EBDA_END, EBDA_START};
use crate::hv::{self, Vcpu, Vm, VmEntry, VmExit};
use crate::loader::{self, linux, ExecType, InitState, Payload};
use crate::mem::{self, Memory};

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::ArchBoard;

#[derive(Debug, Error)]
pub enum Error {
    #[error("hypervisor: {0}")]
    Hv(#[from] hv::Error),

    #[error("memory: {0}")]
    Memory(#[from] mem::Error),

    #[error("loader: {0}")]
    Loader(#[from] loader::Error),

    #[error("cannot handle {0:#x?}")]
    VmExit(String),

    #[error("host io: {0}")]
    HostIo(#[from] std::io::Error),

    #[error("ACPI bytes exceed EBDA area")]
    AcpiTooLong,

    #[error("memory too small")]
    MemoryTooSmall,
}

type Result<T, E = Error> = std::result::Result<T, E>;

pub const STATE_CREATED: u8 = 0;
pub const STATE_RUNNING: u8 = 1;
pub const STATE_SHUTDOWN: u8 = 2;

pub struct BoardConfig {
    pub mem_size: usize,
    pub num_cpu: u32,
}

pub struct Board<V>
where
    V: Vm,
{
    pub vm: V,
    pub memory: Memory,
    pub arch: ArchBoard,
    pub config: BoardConfig,
    pub state: AtomicU8,
    pub payload: RwLock<Option<Payload>>,
}

impl<V> Board<V>
where
    V: Vm,
{
    pub fn create_firmware_data(&self, _init_state: &InitState) -> Result<()> {
        let acpi_bytes = create_acpi_tables(EBDA_START, self.config.num_cpu);
        if acpi_bytes.len() > EBDA_END - EBDA_START {
            return Err(Error::AcpiTooLong);
        }
        let ram = self.memory.ram_bus();
        ram.write_range(EBDA_START, acpi_bytes.len(), &*acpi_bytes)?;
        Ok(())
    }

    fn load_payload(&self) -> Result<InitState, Error> {
        let payload = self.payload.read();
        let Some(payload) = payload.as_ref() else {
            return Ok(InitState::default());
        };
        let mem_regions = self.memory.mem_region_entries();
        let init_state = match payload.exec_type {
            ExecType::Linux => linux::load(
                &self.memory.ram_bus(),
                &mem_regions,
                &payload.executable,
                payload.cmd_line.as_deref(),
                payload.initramfs.as_ref(),
            )?,
        };
        Ok(init_state)
    }

    fn vcpu_loop(&self, vcpu: &mut <V as Vm>::Vcpu, id: u32) -> Result<(), Error> {
        let mut vm_entry = VmEntry::None;
        loop {
            // TODO is there any race here?
            if self.state.load(Ordering::Acquire) == STATE_SHUTDOWN {
                vm_entry = VmEntry::Shutdown;
            }
            let vm_exit = vcpu.run(vm_entry)?;
            vm_entry = match vm_exit {
                VmExit::Io { port, write, size } => self.memory.handle_io(port, write, size)?,
                VmExit::Mmio { addr, write, size } => self.memory.handle_mmio(addr, write, size)?,
                VmExit::Shutdown => {
                    log::info!("vcpu {id} requested shutdown");
                    break Ok(());
                }
                VmExit::Interrupted => VmEntry::None,
                VmExit::Unknown(msg) => break Err(Error::VmExit(msg)),
            };
        }
    }

    fn run_vcpu_inner(
        &self,
        id: u32,
        event_tx: &Sender<u32>,
        boot_rx: &Receiver<()>,
    ) -> Result<(), Error> {
        let mut vcpu = self.vm.create_vcpu(id)?;
        event_tx.send(id).unwrap();
        self.init_vcpu(id, &mut vcpu)?;
        boot_rx.recv().unwrap();
        if self.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Ok(());
        }
        if id == 0 {
            self.create_ram()?;
            let init_state = self.load_payload()?;
            self.init_boot_vcpu(&mut vcpu, &init_state)?;
            self.create_firmware_data(&init_state)?;
        }
        self.vcpu_loop(&mut vcpu, id)
    }

    pub fn run_vcpu(
        &self,
        id: u32,
        event_tx: Sender<u32>,
        boot_rx: Receiver<()>,
    ) -> Result<(), Error> {
        let ret = self.run_vcpu_inner(id, &event_tx, &boot_rx);
        self.state.store(STATE_SHUTDOWN, Ordering::Release);
        event_tx.send(id).unwrap();
        ret
    }
}