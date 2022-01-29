use core::fmt::{Debug, Formatter, Result};

use libvmm::msr::Msr;
use libvmm::svm::flags::{InterruptType, VmcbCleanBits, VmcbIntInfo, VmcbTlbControl};
use libvmm::svm::{vmcb::VmcbSegment, SvmExitCode, SvmIntercept, Vmcb};
use x86::{segmentation, segmentation::SegmentSelector, task};
use x86_64::addr::VirtAddr;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::DescriptorTablePointer;

use crate::arch::segmentation::Segment;
use crate::arch::vmm::VcpuAccessGuestState;
use crate::arch::{GeneralRegisters, GuestPageTableImmut, LinuxContext};
use crate::cell::Cell;
use crate::error::HvResult;
use crate::memory::{addr::virt_to_phys, Frame, GenericPageTableImmut};
use crate::percpu::PerCpu;

#[repr(C)]
pub struct Vcpu {
    /// Save guest general registers when handle VM exits.
    guest_regs: GeneralRegisters,
    /// GS_BASE will be loaded from here when handle VM exits.
    host_tp: u64,
    /// RSP will be loaded from here when handle VM exits.
    host_stack_top: u64,
    /// host state-save area.
    host_save_area: Frame,
    /// Virtual machine control block.
    pub(super) vmcb: Vmcb,
}

impl Vcpu {
    pub fn new(linux: &LinuxContext, cell: &Cell) -> HvResult<Self> {
        super::check_hypervisor_feature()?;

        // make sure all perf counters are off
        unsafe {
            /// Core Performance Event-Select Register (PerfEvtSeln), Counter Enable (bit 22)
            const PERF_EVT_SEL_EN: u64 = 1 << 22;
            Msr::PERF_EVT_SEL0.write(Msr::PERF_EVT_SEL0.read() & !PERF_EVT_SEL_EN);
            Msr::PERF_EVT_SEL1.write(Msr::PERF_EVT_SEL1.read() & !PERF_EVT_SEL_EN);
            Msr::PERF_EVT_SEL2.write(Msr::PERF_EVT_SEL2.read() & !PERF_EVT_SEL_EN);
            Msr::PERF_EVT_SEL3.write(Msr::PERF_EVT_SEL3.read() & !PERF_EVT_SEL_EN);
            Msr::PERF_EVT_SEL4.write(Msr::PERF_EVT_SEL4.read() & !PERF_EVT_SEL_EN);
            Msr::PERF_EVT_SEL5.write(Msr::PERF_EVT_SEL5.read() & !PERF_EVT_SEL_EN);
        }

        // TODO: check linux CR0, CR4

        let efer = Efer::read();
        if efer.contains(EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE) {
            return hv_result_err!(EBUSY, "SVM is already turned on!");
        }
        let host_save_area = Frame::new()?;
        unsafe { Efer::write(efer | EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE) };
        unsafe { Msr::VM_HSAVE_PA.write(host_save_area.start_paddr() as _) };
        info!("successed to turn on SVM.");

        // bring CR0 and CR4 into well-defined states.
        unsafe {
            Cr0::write(super::super::HOST_CR0);
            Cr4::write(super::super::HOST_CR4);
        }

        let cpu_data = PerCpu::current();
        let mut ret = Self {
            guest_regs: Default::default(),
            host_tp: cpu_data as *const _ as _,
            host_stack_top: cpu_data.stack_top() as _,
            host_save_area,
            vmcb: Default::default(),
        };
        ret.vmcb_setup(linux, cell);

        Ok(ret)
    }

    pub fn enter(&mut self, linux: &LinuxContext) -> HvResult {
        let vmcb_paddr = virt_to_phys(&self.vmcb as *const _ as usize);
        let regs = self.regs_mut();
        regs.rax = vmcb_paddr as _;
        regs.rbx = linux.rbx;
        regs.rbp = linux.rbp;
        regs.r12 = linux.r12;
        regs.r13 = linux.r13;
        regs.r14 = linux.r14;
        regs.r15 = linux.r15;
        unsafe {
            asm!(
                "clgi",
                "mov rsp, {0}",
                restore_regs_from_stack!(),
                "vmload rax",
                "jmp {1}",
                in(reg) regs as * const _ as usize,
                sym svm_run,
                options(noreturn),
            );
        }
    }

    pub fn exit(&self, linux: &mut LinuxContext) -> HvResult {
        self.load_vmcb_guest(linux);
        unsafe {
            asm!("stgi");
            Efer::write(Efer::read() - EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE);
            Msr::VM_HSAVE_PA.write(0);
        }
        info!("successed to turn off SVM.");
        Ok(())
    }

    pub fn inject_fault(&mut self) -> HvResult {
        self.vmcb.inject_event(
            VmcbIntInfo::from(
                InterruptType::Exception,
                crate::arch::ExceptionType::GeneralProtectionFault,
            ),
            0,
        );
        Ok(())
    }

    pub fn advance_rip(&mut self, instr_len: u8) -> HvResult {
        self.vmcb.save.rip += instr_len as u64;
        Ok(())
    }

    pub fn guest_is_privileged(&self) -> bool {
        self.vmcb.save.cpl == 0
    }

    pub fn in_hypercall(&self) -> bool {
        use core::convert::TryInto;
        matches!(
            self.vmcb.control.exit_code.try_into(),
            Ok(SvmExitCode::VMMCALL)
        )
    }

    pub fn guest_page_table(&self) -> GuestPageTableImmut {
        use crate::memory::addr::align_down;
        unsafe { GuestPageTableImmut::from_root(align_down(self.vmcb.save.cr3 as _)) }
    }
}

impl Vcpu {
    fn set_vmcb_dtr(vmcb_seg: &mut VmcbSegment, dtr: &DescriptorTablePointer) {
        vmcb_seg.limit = dtr.limit as u32 & 0xffff;
        vmcb_seg.base = dtr.base.as_u64();
    }

    fn set_vmcb_segment(vmcb_seg: &mut VmcbSegment, seg: &Segment) {
        vmcb_seg.selector = seg.selector.bits();
        vmcb_seg.attr = seg.access_rights.as_svm_segment_attributes();
        vmcb_seg.limit = seg.limit;
        vmcb_seg.base = seg.base;
    }

    fn vmcb_setup(&mut self, linux: &LinuxContext, cell: &Cell) {
        self.set_cr(0, linux.cr0.bits());
        self.set_cr(4, linux.cr4.bits());
        self.set_cr(3, linux.cr3);

        let vmcb = &mut self.vmcb.save;
        Self::set_vmcb_segment(&mut vmcb.es, &linux.es);
        Self::set_vmcb_segment(&mut vmcb.cs, &linux.cs);
        Self::set_vmcb_segment(&mut vmcb.ss, &linux.ss);
        Self::set_vmcb_segment(&mut vmcb.ds, &linux.ds);
        Self::set_vmcb_segment(&mut vmcb.fs, &linux.fs);
        Self::set_vmcb_segment(&mut vmcb.gs, &linux.gs);
        Self::set_vmcb_segment(&mut vmcb.tr, &linux.tss);
        Self::set_vmcb_segment(&mut vmcb.ldtr, &Segment::invalid());
        Self::set_vmcb_dtr(&mut vmcb.idtr, &linux.idt);
        Self::set_vmcb_dtr(&mut vmcb.gdtr, &linux.gdt);
        vmcb.cpl = 0; // Linux runs in ring 0 before migration
        vmcb.rflags = 0x2;
        vmcb.rip = linux.rip;
        vmcb.rsp = linux.rsp;
        vmcb.rax = 0;
        vmcb.sysenter_cs = Msr::IA32_SYSENTER_CS.read();
        vmcb.sysenter_eip = Msr::IA32_SYSENTER_EIP.read();
        vmcb.sysenter_esp = Msr::IA32_SYSENTER_ESP.read();
        vmcb.star = linux.star;
        vmcb.lstar = linux.lstar;
        vmcb.cstar = linux.cstar;
        vmcb.sfmask = linux.fmask;
        vmcb.kernel_gs_base = Msr::IA32_KERNEL_GSBASE.read();
        vmcb.efer = linux.efer | EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE.bits(); // Make the hypervisor visible
        vmcb.g_pat = linux.pat;
        vmcb.dr7 = 0x400;
        vmcb.dr6 = 0xffff_0ff0;

        let vmcb = &mut self.vmcb.control;
        vmcb.intercept_exceptions = 0;
        vmcb.np_enable = 1;
        vmcb.guest_asid = 1; // No more than one guest owns the CPU
        vmcb.clean_bits = VmcbCleanBits::empty(); // Explicitly mark all of the state as new
        vmcb.nest_cr3 = cell.gpm.page_table().root_paddr() as _;
        vmcb.tlb_control = VmcbTlbControl::FlushAsid as _;

        self.vmcb.set_intercept(SvmIntercept::NMI);
        self.vmcb.set_intercept(SvmIntercept::CPUID);
        self.vmcb.set_intercept(SvmIntercept::SHUTDOWN);
        self.vmcb.set_intercept(SvmIntercept::VMRUN);
        self.vmcb.set_intercept(SvmIntercept::VMMCALL);
        self.vmcb.set_intercept(SvmIntercept::VMLOAD);
        self.vmcb.set_intercept(SvmIntercept::VMSAVE);
        self.vmcb.set_intercept(SvmIntercept::STGI);
        self.vmcb.set_intercept(SvmIntercept::CLGI);
        self.vmcb.set_intercept(SvmIntercept::SKINIT);
    }

    fn load_vmcb_guest(&self, linux: &mut LinuxContext) {
        let vmcb = &self.vmcb.save;
        linux.rip = vmcb.rip;
        linux.rsp = vmcb.rsp;
        linux.cr0 = Cr0Flags::from_bits_truncate(vmcb.cr0);
        linux.cr3 = vmcb.cr3;
        linux.cr4 = Cr4Flags::from_bits_truncate(vmcb.cr4);
        linux.efer = vmcb.efer & !EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE.bits();

        linux.es.selector = SegmentSelector::from_raw(vmcb.es.selector);
        linux.cs.selector = SegmentSelector::from_raw(vmcb.cs.selector);
        linux.ss.selector = SegmentSelector::from_raw(vmcb.ss.selector);
        linux.ds.selector = SegmentSelector::from_raw(vmcb.ds.selector);

        linux.gdt.base = VirtAddr::new(vmcb.gdtr.base);
        linux.gdt.limit = vmcb.gdtr.limit as _;
        linux.idt.base = VirtAddr::new(vmcb.idtr.base);
        linux.idt.limit = vmcb.idtr.limit as _;

        // We should load the following register state manually since we not use VMLOAD/VMSAVE
        linux.fs.selector = segmentation::fs();
        linux.gs.selector = segmentation::gs();
        linux.tss.selector = unsafe { task::tr() };
        linux.fs.base = Msr::IA32_FS_BASE.read();
        linux.gs.base = vmcb.gs.base;
    }
}

impl VcpuAccessGuestState for Vcpu {
    fn regs(&self) -> &GeneralRegisters {
        &self.guest_regs
    }

    fn regs_mut(&mut self) -> &mut GeneralRegisters {
        &mut self.guest_regs
    }

    fn instr_pointer(&self) -> u64 {
        self.vmcb.save.rip
    }

    fn stack_pointer(&self) -> u64 {
        self.vmcb.save.rsp
    }

    fn set_stack_pointer(&mut self, sp: u64) {
        self.vmcb.save.rsp = sp
    }

    fn rflags(&self) -> u64 {
        self.vmcb.save.rflags
    }

    fn fs_base(&self) -> u64 {
        Msr::IA32_FS_BASE.read()
    }

    fn gs_base(&self) -> u64 {
        self.vmcb.save.gs.base
    }

    fn cr(&self, cr_idx: usize) -> u64 {
        match cr_idx {
            0 => self.vmcb.save.cr0,
            3 => self.vmcb.save.cr3,
            4 => self.vmcb.save.cr4,
            _ => unreachable!(),
        }
    }

    fn set_cr(&mut self, cr_idx: usize, val: u64) {
        match cr_idx {
            0 => self.vmcb.save.cr0 = val & !Cr0Flags::NOT_WRITE_THROUGH.bits(),
            3 => self.vmcb.save.cr3 = val,
            4 => self.vmcb.save.cr4 = val,
            _ => unreachable!(),
        }
    }
}

impl Debug for Vcpu {
    fn fmt(&self, f: &mut Formatter) -> Result {
        f.debug_struct("Vcpu")
            .field("guest_regs", &self.guest_regs)
            .field("rip", &self.instr_pointer())
            .field("rsp", &self.stack_pointer())
            .field("rflags", unsafe {
                &RFlags::from_bits_unchecked(self.rflags())
            })
            .field("cr0", unsafe { &Cr0Flags::from_bits_unchecked(self.cr(0)) })
            .field("cr3", &self.cr(3))
            .field("cr4", unsafe { &Cr4Flags::from_bits_unchecked(self.cr(4)) })
            .field("cs", &self.vmcb.save.cs)
            .finish()
    }
}

#[naked]
unsafe extern "sysv64" fn svm_run() -> ! {
    asm!(
        "vmrun rax",
        save_regs_to_stack!(),
        "mov r14, rax",             // save host RAX to r14 for VMRUN
        "mov r15, rsp",             // save temporary RSP to r15
        "mov rdi, [rsp + {0}]",     // set the first argument to Vcpu::host_tp
        "mov rsp, [rsp + {0} + 8]", // set RSP to Vcpu::host_stack_top
        "call {1}",
        "lea rsp, [r15 + 8]",       // load temporary RSP and skip one place for RAX
        "push r14",                 // push saved RAX to restore RAX later
        restore_regs_from_stack!(),
        "jmp {2}",
        const core::mem::size_of::<GeneralRegisters>(),
        sym vmexit_handler_wrapper,
        sym svm_run,
        options(noreturn),
    );
}

extern "sysv64" fn vmexit_handler_wrapper(cpu_data: &mut PerCpu) {
    // Since VMRUN won't save/restore GS_BASE, we need to do it manually.
    let guest_tp = Msr::IA32_GS_BASE.read();
    cpu_data.vcpu.vmcb.save.gs.base = guest_tp;
    unsafe { Msr::IA32_GS_BASE.write(cpu_data as *const _ as u64) };
    crate::arch::vmm::vmexit_handler();
    unsafe { Msr::IA32_GS_BASE.write(guest_tp) };
}
