#[macro_use]
mod context;
mod apic;
mod cpuid;
mod entry;
mod exception;
mod page_table;
mod segmentation;
mod tables;

pub mod cpu;
pub mod serial;
pub mod vmm;

pub use context::{GuestRegisters, LinuxContext};
pub use exception::ExceptionType;
pub use page_table::PageTable as HostPageTable;
pub use page_table::PageTable as GuestPageTable;
pub use page_table::PageTableImmut as GuestPageTableImmut;
pub use vmm::NestedPageTable;

pub fn init_early() -> crate::error::HvResult {
    apic::init()
}
