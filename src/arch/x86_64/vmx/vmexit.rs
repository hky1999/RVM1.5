use libvmm::vmx::vmcs::{EptViolationInfo, ExitInterruptionInfo, VmExitInfo};
use libvmm::vmx::VmxExitReason;

use crate::arch::exception::ExceptionType;
use crate::arch::vmm::VmExit;
use crate::error::HvResult;

impl VmExit<'_> {
    fn handle_exception_nmi(&mut self, exit_info: &VmExitInfo) -> HvResult {
        let intr_info = ExitInterruptionInfo::new()?;
        info!(
            "VM exit: Exception or NMI @ RIP({:#x}, {}): {:#x?}",
            exit_info.guest_rip, exit_info.exit_instruction_length, intr_info
        );
        match intr_info.vector {
            ExceptionType::NonMaskableInterrupt => unsafe {
                asm!("int {}", const ExceptionType::NonMaskableInterrupt)
            },
            v => warn!("Unhandled Guest Exception: #{:#x}", v),
        }
        Ok(())
    }

    fn handle_ept_violation(&mut self, exit_info: &VmExitInfo) -> HvResult {
        let ept_vio_info = EptViolationInfo::new()?;
        warn!(
            "VM exit: EPT violation @ {:#x} RIP({:#x}, {}): {:#x?}",
            ept_vio_info.guest_paddr,
            exit_info.guest_rip,
            exit_info.exit_instruction_length,
            ept_vio_info
        );
        hv_result_err!(ENOSYS)
    }

    pub fn handle_exit(&mut self) -> HvResult {
        let exit_info = VmExitInfo::new()?;
        trace!("VM exit: {:#x?}", exit_info);

        if exit_info.entry_failure {
            panic!("VM entry failed: {:#x?}", exit_info);
        }
        // self.test_read_guest_memory(
        //     exit_info.guest_rip as _,
        //     exit_info.exit_instruction_length as _,
        // )?;

        let res = match exit_info.exit_reason {
            VmxExitReason::EXCEPTION_NMI => self.handle_exception_nmi(&exit_info),
            VmxExitReason::CPUID => self.handle_cpuid(),
            VmxExitReason::VMCALL => self.handle_hypercall(),
            VmxExitReason::MSR_READ => self.handle_msr_read(),
            VmxExitReason::MSR_WRITE => self.handle_msr_write(),
            VmxExitReason::EPT_VIOLATION => self.handle_ept_violation(&exit_info),
            VmxExitReason::TRIPLE_FAULT => panic!("Triple fault!"),
            _ => hv_result_err!(ENOSYS),
        };

        if res.is_err() {
            warn!(
                "VM exit handler for reason {:?} returned {:?}:\n\
                {:#x?}\n\n\
                Guest State Dump:\n\
                {:#x?}",
                exit_info.exit_reason, res, exit_info, self.cpu_data.vcpu,
            );
        }
        res
    }
}
