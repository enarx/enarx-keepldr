// SPDX-License-Identifier: Apache-2.0

//! syscall interface layer between assembler and rust

use crate::addr::{HostVirtAddr, ShimPhysUnencryptedAddr};
use crate::allocator::{AllocateError, ALLOCATOR};
use crate::asm::_enarx_asm_triple_fault;
use crate::attestation::SEV_SECRET;
use crate::hostcall::{HostCall, HOST_CALL_ALLOC};
use crate::paging::SHIM_PAGETABLE;
use crate::payload::{NEXT_BRK_RWLOCK, NEXT_MMAP_RWLOCK};
use crate::{eprintln, C_BIT_MASK};
use core::convert::TryFrom;
use core::mem::size_of;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::Ordering;
use primordial::{Address, Register};
use sallyport::{Cursor, Request};
use syscall::{
    BaseSyscallHandler, EnarxSyscallHandler, FileSyscallHandler, MemorySyscallHandler,
    NetworkSyscallHandler, ProcessSyscallHandler, SyscallHandler, SystemSyscallHandler,
    ARCH_GET_FS, ARCH_GET_GS, ARCH_SET_FS, ARCH_SET_GS, SEV_TECH,
};
use untrusted::{AddressValidator, UntrustedRef, UntrustedRefMut, Validate, ValidateSlice};
use x86_64::instructions::tlb::flush_all;
use x86_64::registers::{rdfsbase, rdgsbase, wrfsbase, wrgsbase};
use x86_64::structures::paging::mapper::{MapToError, MappedFrame, TranslateResult};
use x86_64::structures::paging::{
    Mapper, Page, PageTableFlags, Size1GiB, Size2MiB, Size4KiB, Translate,
};
use x86_64::{align_up, VirtAddr};

#[repr(C)]
struct X8664DoubleReturn {
    rax: u64,
    rdx: u64,
}

/// syscall service routine
///
/// # Safety
///
/// This function is not be called from rust.
#[inline(never)]
#[naked]
pub unsafe extern "sysv64" fn _syscall_enter() -> ! {
    use crate::gdt::{USER_CODE_SEGMENT, USER_DATA_SEGMENT};
    // TaskStateSegment.privilege_stack_table[0]
    const KERNEL_RSP_OFF: usize = size_of::<u32>();
    // TaskStateSegment.privilege_stack_table[3]
    const USR_RSP_OFF: usize = size_of::<u32>() + 3 * size_of::<u64>();

    asm!("
    # prepare the stack for iretq and load the kernel rsp
    swapgs                                             # set gs segment to TSS
    mov    QWORD PTR gs:{USR},      rsp                # save userspace rsp
    mov    rsp,                     QWORD PTR gs:{KRN} # load kernel rsp
    push   {USER_DATA_SEGMENT}
    push   QWORD PTR gs:{USR}                          # push userspace rsp - stack_pointer_ring_3
    mov    QWORD PTR gs:{USR},      0x0                # clear userspace rsp in the TSS
    push   r11                                         # push RFLAGS stored in r11
    push   {USER_CODE_SEGMENT}
    push   rcx                                         # push userspace return pointer

    push   rbx
    mov    rbx, rsp

    # Arguments in registers:
    # SYSV:    rdi, rsi, rdx, rcx, r8, r9
    # SYSCALL: rdi, rsi, rdx, r10, r8, r9 and syscall number in rax
    mov    rcx,                     r10

    # These will be preserved by `syscall_rust` via the SYS-V ABI
    # rbx, rsp, rbp, r12, r13, r14, r15

    # save registers
    push   rdi
    push   rsi
    push   rdx
    push   r11
    push   r10
    push   r8
    push   r9

    # syscall number on the stack as the seventh argument
    push   rax

    call   {syscall_rust}

    # skip %rax pop, as it is the return value
    add    rsp,                     0x8

    # restore registers
    pop    r9
    pop    r8
    pop    r10
    pop    r11
    add    rsp,                     0x8               # skip rdx, because it is a return value
    pop    rsi
    pop    rdi

    pop    rbx

    xor    rcx,                     rcx               # do not leak contents to userspace
    xor    r11,                     r11               # do not leak contents to userspace

    swapgs                                            # restore gs

    iretq
    ",
    USR = const USR_RSP_OFF,
    KRN = const KERNEL_RSP_OFF,
    USER_DATA_SEGMENT = const USER_DATA_SEGMENT,
    USER_CODE_SEGMENT = const USER_CODE_SEGMENT,
    syscall_rust = sym syscall_rust,
    options(noreturn)
    );
}

/// Handle a syscall in rust
#[allow(clippy::many_single_char_names)]
extern "sysv64" fn syscall_rust(
    a: Register<usize>,
    b: Register<usize>,
    c: Register<usize>,
    d: Register<usize>,
    e: Register<usize>,
    f: Register<usize>,
    nr: usize,
) -> X8664DoubleReturn {
    let orig_rdx: usize = c.into();

    let mut h = Handler {
        hostcall: HOST_CALL_ALLOC.try_alloc().unwrap(),
        argv: [a.into(), b.into(), c.into(), d.into(), e.into(), f.into()],
    };

    let ret = h.syscall(a, b, c, d, e, f, nr);

    match ret {
        Err(e) => X8664DoubleReturn {
            rax: e.checked_neg().unwrap() as _,
            // Preserve `rdx` as it is normally not clobbered with a syscall
            rdx: orig_rdx as _,
        },
        Ok([rax, rdx]) => X8664DoubleReturn {
            rax: rax.into(),
            rdx: rdx.into(),
        },
    }
}

/// The syscall Handler
struct Handler {
    hostcall: HostCall,
    argv: [usize; 6],
}

impl AddressValidator for Handler {
    #[inline(always)]
    fn validate_const_mem_fn(&self, _ptr: *const (), _size: usize) -> bool {
        // FIXME: https://github.com/enarx/enarx/issues/630
        true
    }

    #[inline(always)]
    fn validate_mut_mem_fn(&self, _ptr: *mut (), _size: usize) -> bool {
        // FIXME: https://github.com/enarx/enarx/issues/630
        true
    }
}

impl SyscallHandler for Handler {}
impl SystemSyscallHandler for Handler {}
impl NetworkSyscallHandler for Handler {}
impl FileSyscallHandler for Handler {}

impl BaseSyscallHandler for Handler {
    fn unknown_syscall(
        &mut self,
        _a: Register<usize>,
        _b: Register<usize>,
        _c: Register<usize>,
        _d: Register<usize>,
        _e: Register<usize>,
        _f: Register<usize>,
        nr: usize,
    ) {
        eprintln!("unsupported syscall: {}", nr);
    }

    unsafe fn proxy(&mut self, req: Request) -> sallyport::Result {
        let block = self.hostcall.as_mut_block();
        block.msg.req = req;
        self.hostcall.hostcall()
    }

    fn attacked(&mut self) -> ! {
        // provoke triple fault, causing a VM shutdown
        unsafe { _enarx_asm_triple_fault() };
    }

    fn translate_shim_to_host_addr<T>(buf: *const T) -> usize {
        let buf_address = Address::from(buf);
        let phys_unencrypted = ShimPhysUnencryptedAddr::try_from(buf_address).unwrap();
        Register::<usize>::from(HostVirtAddr::from(phys_unencrypted)).into()
    }

    fn new_cursor(&mut self) -> Cursor {
        self.hostcall.as_mut_block().cursor()
    }

    fn trace(&mut self, name: &str, argc: usize) {
        eprint!("{}(", name);
        for (i, arg) in self.argv[..argc].iter().copied().enumerate() {
            let prefix = if i > 0 { ", " } else { "" };
            eprint!("{}{:#x}", prefix, arg);
        }

        eprintln!(")");
    }
}

impl EnarxSyscallHandler for Handler {
    fn get_attestation(
        &mut self,
        _nonce: UntrustedRef<u8>,
        _nonce_len: libc::size_t,
        buf: UntrustedRefMut<u8>,
        buf_len: libc::size_t,
    ) -> sallyport::Result {
        self.trace("get_attestation", 4);

        let secret = SEV_SECRET.read();

        match secret.try_len() {
            Some(mut result_len) => {
                if buf_len != 0 {
                    result_len = result_len.min(buf_len);
                    let buf = buf.validate_slice(buf_len, self).ok_or(libc::EFAULT)?;

                    buf[..result_len].copy_from_slice(
                        &(SEV_SECRET.read()).try_as_slice().unwrap()[..result_len],
                    );
                }

                Ok([result_len.into(), SEV_TECH.into()])
            }
            None => {
                if C_BIT_MASK.load(Ordering::Relaxed) == 0 {
                    Err(libc::ENOSYS)
                } else {
                    Ok([0.into(), SEV_TECH.into()])
                }
            }
        }
    }
}

impl ProcessSyscallHandler for Handler {
    fn arch_prctl(&mut self, code: i32, addr: u64) -> sallyport::Result {
        self.trace("arch_prctl", 2);
        match code {
            ARCH_SET_FS => {
                // FIXME: check `addr` value
                unsafe {
                    wrfsbase(addr);
                }
                eprintln!("SC> arch_prctl(ARCH_SET_FS, {:#x}) = 0", addr);
                Ok(Default::default())
            }
            ARCH_GET_FS => {
                let addr = UntrustedRefMut::from(addr as *mut libc::c_ulong);
                let addr = addr.validate(self).ok_or(libc::EFAULT)?;
                unsafe {
                    *addr = rdfsbase();
                }
                Ok(Default::default())
            }
            ARCH_SET_GS => {
                // FIXME: check `addr` value
                unsafe {
                    wrgsbase(addr);
                }
                eprintln!("SC> arch_prctl(ARCH_SET_GS, {:#x}) = 0", addr);
                Ok(Default::default())
            }
            ARCH_GET_GS => {
                let addr = UntrustedRefMut::from(addr as *mut libc::c_ulong);
                let addr = addr.validate(self).ok_or(libc::EFAULT)?;
                unsafe {
                    *addr = rdgsbase();
                }
                Ok(Default::default())
            }
            x => {
                eprintln!("SC> arch_prctl({:#x}, {:#x}) = -EINVAL", x, addr);
                Err(libc::EINVAL)
            }
        }
    }
}

impl MemorySyscallHandler for Handler {
    fn mprotect(&mut self, addr: UntrustedRef<u8>, len: usize, prot: i32) -> sallyport::Result {
        self.trace("mprotect", 3);
        let addr = addr.as_ptr();

        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;

        if prot & libc::PROT_WRITE != 0 {
            flags |= PageTableFlags::WRITABLE;
        }

        if prot & libc::PROT_EXEC == 0 {
            flags |= PageTableFlags::NO_EXECUTE;
        }

        let mut page_table = SHIM_PAGETABLE.write();

        let start_addr = VirtAddr::from_ptr(addr);
        let start_page: Page = Page::containing_address(start_addr);
        let end_page: Page = Page::containing_address(start_addr + len - 1u64);
        let page_range = Page::range_inclusive(start_page, end_page);
        for page in page_range {
            unsafe {
                match page_table.update_flags(page, flags) {
                    Ok(flush) => flush.ignore(),
                    Err(e) => {
                        eprintln!(
                            "SC> mprotect({:#?}, {}, {}, …) = EINVAL ({:#?})",
                            addr, len, prot, e
                        );
                        return Err(libc::EINVAL);
                    }
                }
            }
        }

        flush_all();

        eprintln!("SC> mprotect({:#?}, {}, {}, …) = 0", addr, len, prot);

        Ok(Default::default())
    }

    fn mmap(
        &mut self,
        addr: UntrustedRef<u8>,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> sallyport::Result {
        self.trace("mmap", 6);

        const PA: i32 = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

        match (addr.as_ptr(), length, prot, flags, fd, offset) {
            (ptr, _, _, PA, -1, 0) if ptr.is_null() => {
                let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;

                if prot & libc::PROT_WRITE != 0 {
                    flags |= PageTableFlags::WRITABLE;
                }

                if prot & libc::PROT_EXEC == 0 {
                    flags |= PageTableFlags::NO_EXECUTE;
                }

                let virt_addr = *NEXT_MMAP_RWLOCK.read().deref();
                let len_aligned = align_up(length as _, Page::<Size4KiB>::SIZE) as _;

                let mem_slice = ALLOCATOR
                    .write()
                    .allocate_and_map_memory(
                        SHIM_PAGETABLE.write().deref_mut(),
                        virt_addr,
                        len_aligned,
                        flags,
                        PageTableFlags::PRESENT
                            | PageTableFlags::WRITABLE
                            | PageTableFlags::USER_ACCESSIBLE,
                    )
                    .map_err(|_| {
                        eprintln!("SC> mmap(0, {}, …) = ENOMEM", length);
                        libc::ENOMEM
                    })?;
                eprintln!("SC> mmap(0, {}, …) = {:#?}", length, mem_slice.as_ptr());
                unsafe {
                    core::ptr::write_bytes(mem_slice.as_mut_ptr(), 0, length);
                }
                *NEXT_MMAP_RWLOCK.write().deref_mut() = virt_addr + (len_aligned as u64);

                //eprintln!("next_mmap = {:#x}", *NEXT_MMAP_RWLOCK::read().deref());

                Ok([mem_slice.as_ptr().into(), Default::default()])
            }
            (addr, ..) => {
                eprintln!("SC> mmap({:#?}, {}, …)", addr, length);
                unimplemented!()
            }
        }
    }

    fn mremap(
        &mut self,
        old_addr: UntrustedRefMut<u8>,
        old_size: libc::size_t,
        new_size: libc::size_t,
        flags: libc::c_int,
        _new_addr: UntrustedRef<u8>,
    ) -> sallyport::Result {
        self.trace("mremap", 5);

        if old_size >= new_size {
            return Err(libc::EINVAL);
        }

        let mut target_addr = old_addr
            .validate_slice(old_size, self)
            .ok_or(libc::EINVAL)?;

        target_addr = unsafe {
            if (flags & libc::MREMAP_FIXED) != libc::MREMAP_FIXED
                && (flags & libc::MREMAP_MAYMOVE) == libc::MREMAP_MAYMOVE
            {
                let add_addr = target_addr.as_ptr().add(old_size);
                let mut mapper = SHIM_PAGETABLE.write();
                let mut allocator = ALLOCATOR.write();

                match allocator.allocate_and_map_memory(
                    mapper.deref_mut(),
                    VirtAddr::from_ptr(add_addr),
                    new_size - old_size,
                    PageTableFlags::PRESENT
                        | PageTableFlags::USER_ACCESSIBLE
                        | PageTableFlags::WRITABLE,
                    PageTableFlags::PRESENT
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::USER_ACCESSIBLE,
                ) {
                    Err(AllocateError::NotAligned) => Err(libc::EINVAL),
                    Err(AllocateError::OutOfMemory) => Err(libc::ENOMEM),
                    Err(AllocateError::ZeroSize) => Err(libc::EINVAL),
                    Err(AllocateError::PageAlreadyMapped)
                    | Err(AllocateError::ParentEntryHugePage) => {
                        // remap the whole thing
                        let mut new_target_addr = *NEXT_MMAP_RWLOCK.read().deref();

                        while mapper.translate_addr(new_target_addr).is_some() {
                            new_target_addr += Page::<Size1GiB>::SIZE;
                        }

                        let mut to_map = old_size as u64;
                        let mut old_addr = VirtAddr::from_ptr(target_addr.as_ptr());

                        let mut new_addr = new_target_addr;

                        loop {
                            let mapped = match mapper.translate(old_addr) {
                                TranslateResult::Mapped {
                                    frame: MappedFrame::Size4KiB(frame),
                                    offset: 0,
                                    ..
                                } => {
                                    mapper
                                        .map_to(
                                            Page::<Size4KiB>::containing_address(new_addr),
                                            frame,
                                            PageTableFlags::PRESENT
                                                | PageTableFlags::USER_ACCESSIBLE
                                                | PageTableFlags::WRITABLE,
                                            allocator.deref_mut(),
                                        )
                                        .map_err(|e| match e {
                                            MapToError::FrameAllocationFailed => libc::ENOMEM,
                                            MapToError::ParentEntryHugePage => libc::EINVAL,
                                            MapToError::PageAlreadyMapped(_) => libc::EINVAL,
                                        })?
                                        .flush();
                                    Page::<Size4KiB>::SIZE
                                }
                                TranslateResult::Mapped {
                                    frame: MappedFrame::Size2MiB(frame),
                                    offset: 0,
                                    ..
                                } => {
                                    mapper
                                        .map_to(
                                            Page::<Size2MiB>::containing_address(new_addr),
                                            frame,
                                            PageTableFlags::PRESENT
                                                | PageTableFlags::USER_ACCESSIBLE
                                                | PageTableFlags::WRITABLE,
                                            allocator.deref_mut(),
                                        )
                                        .map_err(|e| match e {
                                            MapToError::FrameAllocationFailed => libc::ENOMEM,
                                            MapToError::ParentEntryHugePage => libc::EINVAL,
                                            MapToError::PageAlreadyMapped(_) => libc::EINVAL,
                                        })?
                                        .flush();
                                    Page::<Size2MiB>::SIZE
                                }
                                TranslateResult::Mapped {
                                    frame: MappedFrame::Size1GiB(frame),
                                    offset: 0,
                                    ..
                                } => {
                                    mapper
                                        .map_to(
                                            Page::<Size1GiB>::containing_address(new_addr),
                                            frame,
                                            PageTableFlags::PRESENT
                                                | PageTableFlags::USER_ACCESSIBLE
                                                | PageTableFlags::WRITABLE,
                                            allocator.deref_mut(),
                                        )
                                        .map_err(|e| match e {
                                            MapToError::FrameAllocationFailed => libc::ENOMEM,
                                            MapToError::ParentEntryHugePage => libc::EINVAL,
                                            MapToError::PageAlreadyMapped(_) => libc::EINVAL,
                                        })?
                                        .flush();
                                    Page::<Size1GiB>::SIZE
                                }
                                _ => return Err(libc::EINVAL),
                            };

                            old_addr += mapped;
                            new_addr += mapped;

                            if to_map <= mapped {
                                break;
                            }

                            to_map -= mapped;
                        }

                        let _p = allocator
                            .allocate_and_map_memory(
                                mapper.deref_mut(),
                                new_addr,
                                new_size - old_size,
                                PageTableFlags::PRESENT
                                    | PageTableFlags::USER_ACCESSIBLE
                                    | PageTableFlags::WRITABLE,
                                PageTableFlags::PRESENT
                                    | PageTableFlags::WRITABLE
                                    | PageTableFlags::USER_ACCESSIBLE,
                            )
                            .map_err(|e| match e {
                                AllocateError::NotAligned => libc::EINVAL,
                                AllocateError::OutOfMemory => libc::ENOMEM,
                                AllocateError::ZeroSize => libc::EINVAL,
                                AllocateError::PageAlreadyMapped => libc::EINVAL,
                                AllocateError::ParentEntryHugePage => libc::EINVAL,
                            })?;

                        let mut old_addr = VirtAddr::from_ptr(target_addr.as_ptr());
                        let mut to_map = old_size as u64;

                        loop {
                            let mapped = match mapper.translate(old_addr) {
                                TranslateResult::Mapped {
                                    frame: MappedFrame::Size4KiB(_),
                                    offset: 0,
                                    ..
                                } => {
                                    let (_, flush) = mapper
                                        .unmap(Page::<Size4KiB>::containing_address(old_addr))
                                        .map_err(
                                            |_| libc::EINVAL, // FIXME
                                        )?;
                                    flush.flush();
                                    Page::<Size4KiB>::SIZE
                                }
                                TranslateResult::Mapped {
                                    frame: MappedFrame::Size2MiB(_),
                                    offset: 0,
                                    ..
                                } => {
                                    let (_, flush) = mapper
                                        .unmap(Page::<Size2MiB>::containing_address(old_addr))
                                        .map_err(
                                            |_| libc::EINVAL, // FIXME
                                        )?;
                                    flush.flush();
                                    Page::<Size2MiB>::SIZE
                                }
                                TranslateResult::Mapped {
                                    frame: MappedFrame::Size1GiB(_),
                                    offset: 0,
                                    ..
                                } => {
                                    let (_, flush) = mapper
                                        .unmap(Page::<Size1GiB>::containing_address(old_addr))
                                        .map_err(
                                            |_| libc::EINVAL, // FIXME
                                        )?;
                                    flush.flush();
                                    Page::<Size1GiB>::SIZE
                                }
                                _ => return Err(libc::EINVAL),
                            };

                            old_addr += mapped;

                            if to_map <= mapped {
                                break;
                            }

                            to_map -= mapped;
                        }

                        let len_aligned =
                            align_up((new_size - old_size) as _, Page::<Size1GiB>::SIZE);
                        *NEXT_MMAP_RWLOCK.write().deref_mut() = new_target_addr + len_aligned;

                        new_target_addr
                            .as_mut_ptr::<u8>()
                            .as_mut()
                            .ok_or(libc::EINVAL)
                            .map(|v| core::slice::from_raw_parts_mut(v, new_size))
                    }
                    Ok(_p) => Ok(target_addr),
                }?
            } else {
                return Err(libc::EINVAL);
            }
        };

        eprintln!("SC> mremap() = {:?}", target_addr.as_ptr());
        Ok([target_addr.into(), Default::default()])
    }

    fn munmap(&mut self, addr: UntrustedRef<u8>, length: usize) -> sallyport::Result {
        self.trace("munmap", 2);

        let addr = addr.validate_slice(length, self).ok_or(libc::EINVAL)?;

        ALLOCATOR
            .write()
            .unmap_memory(
                SHIM_PAGETABLE.write().deref_mut(),
                VirtAddr::from_ptr(addr.as_ptr()),
                length,
            )
            .map_err(|_| libc::EINVAL)?;

        Ok(Default::default())
    }

    fn brk(&mut self, addr: *const u8) -> sallyport::Result {
        self.trace("brk", 1);
        let len;

        let next_brk = *NEXT_BRK_RWLOCK.read().deref();
        let virt_addr = next_brk;

        match addr as usize {
            0 => {
                eprintln!("SC> brk({:#?}) = {:#x}", addr, next_brk.as_u64());
                Ok([next_brk.as_u64().into(), Default::default()])
            }
            n => {
                if n <= next_brk.as_u64() as usize {
                    if n > (next_brk
                        .as_u64()
                        .checked_sub(Page::<Size4KiB>::SIZE)
                        .unwrap() as usize)
                    {
                        // already mapped
                        eprintln!("SC> brk({:#?}) = {:#x}", addr, n);
                        return Ok([n.into(), Default::default()]);
                    }

                    // n most likely wrong
                    return Err(libc::EINVAL);
                }

                len = n
                    .checked_sub(next_brk.as_u64() as usize)
                    .ok_or(libc::EINVAL)?;
                let len_aligned = align_up(len as _, Page::<Size4KiB>::SIZE) as _;
                let _ = ALLOCATOR
                    .write()
                    .allocate_and_map_memory(
                        SHIM_PAGETABLE.write().deref_mut(),
                        virt_addr,
                        len_aligned,
                        PageTableFlags::PRESENT
                            | PageTableFlags::USER_ACCESSIBLE
                            | PageTableFlags::WRITABLE,
                        PageTableFlags::PRESENT
                            | PageTableFlags::WRITABLE
                            | PageTableFlags::USER_ACCESSIBLE,
                    )
                    .map_err(|_| {
                        eprintln!("SC> brk({:#?}) = ENOMEM", addr);
                        libc::ENOMEM
                    })?;

                *NEXT_BRK_RWLOCK.write() = virt_addr + (len_aligned as u64);

                eprintln!("SC> brk({:#?}) = {:#x}", addr, n);

                Ok([n.into(), Default::default()])
            }
        }
    }

    fn madvise(
        &mut self,
        _addr: *const libc::c_void,
        _length: usize,
        _advice: i32,
    ) -> sallyport::Result {
        self.trace("madvise", 3);
        Ok(Default::default())
    }
}
