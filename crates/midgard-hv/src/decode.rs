//! Emulation of MMIO instructions for backends whose hypervisor does not decode them
//! (Hel): guest page table walks and decoding of the instructions that access device
//! memory. The exit has the guest-physical address, so the backend only needs the
//! direction, the size and the register / immediate operand.

use crate::x86::{CR0_PG, CR4_LA57, CR4_PAE, CR4_PSE, EFER_LMA, PTE_LARGE, PTE_PRESENT};
use crate::{BasicRegs, GuestMemory, Regs};

const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Translates a guest-linear address through the guest's page tables.
pub(crate) fn translate(memory: &GuestMemory, regs: &Regs, linear: u64) -> Option<u64> {
    if regs.cr0 & CR0_PG == 0 {
        return Some(linear & 0xFFFF_FFFF);
    }
    if regs.efer & EFER_LMA != 0 {
        if regs.cr4 & CR4_LA57 != 0 {
            return None;
        }
        let mut table = regs.cr3 & ADDRESS_MASK;
        // PML4, PDPT, PD, PT: 9 bits each, starting at bit 39.
        for level in (0..4).rev() {
            let shift = 12 + 9 * level;
            let index = (linear >> shift) & 0x1FF;
            let entry = memory.read_u64(table + index * 8).ok()?;
            if entry & PTE_PRESENT == 0 {
                return None;
            }
            // 1 GiB pages in the PDPT, 2 MiB pages in the PD.
            if (level == 2 || level == 1) && entry & PTE_LARGE != 0 {
                let page_mask = (1u64 << shift) - 1;
                return Some((entry & ADDRESS_MASK & !page_mask) | (linear & page_mask));
            }
            table = entry & ADDRESS_MASK;
        }
        return Some(table | (linear & 0xFFF));
    }
    let linear = linear & 0xFFFF_FFFF;
    if regs.cr4 & CR4_PAE != 0 {
        let pdpte = memory
            .read_u64((regs.cr3 & 0xFFFF_FFE0) + ((linear >> 30) & 3) * 8)
            .ok()?;
        if pdpte & PTE_PRESENT == 0 {
            return None;
        }
        let pde = memory
            .read_u64((pdpte & ADDRESS_MASK) + ((linear >> 21) & 0x1FF) * 8)
            .ok()?;
        if pde & PTE_PRESENT == 0 {
            return None;
        }
        if pde & PTE_LARGE != 0 {
            return Some((pde & ADDRESS_MASK & !0x1F_FFFF) | (linear & 0x1F_FFFF));
        }
        let pte = memory
            .read_u64((pde & ADDRESS_MASK) + ((linear >> 12) & 0x1FF) * 8)
            .ok()?;
        if pte & PTE_PRESENT == 0 {
            return None;
        }
        return Some((pte & ADDRESS_MASK) | (linear & 0xFFF));
    }
    let read_u32 = |address: u64| {
        let mut bytes = [0u8; 4];
        memory
            .read(address, &mut bytes)
            .ok()
            .map(|()| u32::from_le_bytes(bytes) as u64)
    };
    let pde = read_u32((regs.cr3 & 0xFFFF_F000) + ((linear >> 22) & 0x3FF) * 4)?;
    if pde & PTE_PRESENT == 0 {
        return None;
    }
    if regs.cr4 & CR4_PSE != 0 && pde & PTE_LARGE != 0 {
        return Some((pde & 0xFFC0_0000) | (linear & 0x3F_FFFF));
    }
    let pte = read_u32((pde & 0xFFFF_F000) + ((linear >> 12) & 0x3FF) * 4)?;
    if pte & PTE_PRESENT == 0 {
        return None;
    }
    Some((pte & 0xFFFF_F000) | (linear & 0xFFF))
}

/// Reads up to 15 instruction bytes at CS:RIP. Returns the number of bytes read
/// (fewer if the instruction is at the end of mapped memory).
pub(crate) fn fetch(memory: &GuestMemory, regs: &Regs, bytes: &mut [u8; 15]) -> usize {
    let long_mode = regs.efer & EFER_LMA != 0 && regs.cs.l != 0;
    let mut linear = if long_mode {
        regs.basic.rip
    } else {
        regs.cs.base.wrapping_add(regs.basic.rip)
    };
    let mut done = 0;
    while done < bytes.len() {
        let Some(physical) = translate(memory, regs, linear) else {
            break;
        };
        let chunk = ((0x1000 - (linear & 0xFFF)) as usize).min(bytes.len() - done);
        if memory
            .read(physical, &mut bytes[done..done + chunk])
            .is_err()
        {
            break;
        }
        done += chunk;
        linear = linear.wrapping_add(chunk as u64);
    }
    done
}

/// Why an instruction cannot be emulated.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("16-bit code is not supported")]
    RealMode,
    #[error("truncated instruction")]
    Truncated,
    #[error("instruction is too long")]
    TooLong,
    #[error("REP prefixes are not supported for MMIO")]
    Rep,
    #[error("16-bit addressing is not supported")]
    Address16,
    #[error("register operand in an MMIO access")]
    RegisterOperand,
    #[error("unsupported instruction {0:02x?}")]
    Unsupported(Vec<u8>),
}

/// What an MMIO instruction does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Operation {
    /// Reads `size` bytes into a register, extending them to `dest_size` bytes.
    Load {
        size: u8,
        register: u8,
        /// AH, CH, DH or BH.
        high_byte: bool,
        dest_size: u8,
        sign_extend: bool,
    },
    /// Writes `size` bytes of `value`.
    Store { size: u8, value: u64 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Instruction {
    pub(crate) length: u8,
    pub(crate) operation: Operation,
}

/// Returns the register with the given encoding (0 = RAX, ..., 15 = R15).
pub(crate) fn register(regs: &BasicRegs, index: u8) -> u64 {
    match index {
        0 => regs.rax,
        1 => regs.rcx,
        2 => regs.rdx,
        3 => regs.rbx,
        4 => regs.rsp,
        5 => regs.rbp,
        6 => regs.rsi,
        7 => regs.rdi,
        8 => regs.r8,
        9 => regs.r9,
        10 => regs.r10,
        11 => regs.r11,
        12 => regs.r12,
        13 => regs.r13,
        14 => regs.r14,
        _ => regs.r15,
    }
}

fn register_mut(regs: &mut BasicRegs, index: u8) -> &mut u64 {
    match index {
        0 => &mut regs.rax,
        1 => &mut regs.rcx,
        2 => &mut regs.rdx,
        3 => &mut regs.rbx,
        4 => &mut regs.rsp,
        5 => &mut regs.rbp,
        6 => &mut regs.rsi,
        7 => &mut regs.rdi,
        8 => &mut regs.r8,
        9 => &mut regs.r9,
        10 => &mut regs.r10,
        11 => &mut regs.r11,
        12 => &mut regs.r12,
        13 => &mut regs.r13,
        14 => &mut regs.r14,
        _ => &mut regs.r15,
    }
}

fn mask(size: u8) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * size)) - 1
    }
}

/// Completes a load: writes `value` (of the access size) to the destination register.
pub(crate) fn complete_load(regs: &mut BasicRegs, operation: &Operation, value: u64) {
    let Operation::Load {
        size,
        register,
        high_byte,
        dest_size,
        sign_extend,
    } = *operation
    else {
        return;
    };
    let mut value = value & mask(size);
    if sign_extend && size < 8 {
        let shift = 64 - 8 * size as u32;
        value = (((value << shift) as i64) >> shift) as u64;
    }
    let target = register_mut(regs, register);
    *target = match dest_size {
        1 if high_byte => (*target & !0xFF00) | ((value & 0xFF) << 8),
        1 | 2 => (*target & !mask(dest_size)) | (value & mask(dest_size)),
        // 32-bit destinations zero-extend into the full register.
        4 => value & 0xFFFF_FFFF,
        _ => value,
    };
}

/// Decodes an instruction that accesses memory (32- or 64-bit code).
pub(crate) fn decode(bytes: &[u8], regs: &Regs) -> Result<Instruction, Error> {
    let long_mode = regs.efer & EFER_LMA != 0 && regs.cs.l != 0;
    if !long_mode && regs.cs.db == 0 {
        return Err(Error::RealMode);
    }
    let byte = |i: usize| bytes.get(i).copied().ok_or(Error::Truncated);

    let mut i = 0;
    let mut operand16 = false;
    let mut address32 = false;
    loop {
        match byte(i)? {
            0x66 => operand16 = true,
            0x67 if long_mode => address32 = true,
            // In 32-bit code, it selects 16-bit addressing (with a different ModRM layout).
            0x67 => return Err(Error::Address16),
            // Segment overrides (the exit already has the address) and LOCK.
            0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 | 0xF0 => {}
            0xF2 | 0xF3 => return Err(Error::Rep),
            _ => break,
        }
        i += 1;
    }
    let mut rex = 0u8;
    if long_mode && (0x40..=0x4F).contains(&byte(i)?) {
        rex = byte(i)?;
        i += 1;
    }
    let rex_w = rex & 0x8 != 0;
    let rex_r = (rex & 0x4) << 1;
    let operand_size: u8 = if rex_w {
        8
    } else if operand16 {
        2
    } else {
        4
    };

    // Returns (reg field including REX.R, length of ModRM + SIB + displacement).
    let modrm = |at: usize| -> Result<(u8, usize), Error> {
        let m = byte(at)?;
        let mode = m >> 6;
        let rm = m & 7;
        if mode == 3 {
            return Err(Error::RegisterOperand);
        }
        let mut length = 1;
        if rm == 4 {
            let sib = byte(at + 1)?;
            length += 1;
            if mode == 0 && sib & 7 == 5 {
                length += 4;
            }
        }
        if mode == 0 && rm == 5 {
            length += 4;
        }
        match mode {
            1 => length += 1,
            2 => length += 4,
            _ => {}
        }
        Ok((((m >> 3) & 7) | rex_r, length))
    };
    // 8-bit register operands: without REX, encodings 4-7 are AH, CH, DH and BH.
    let byte_register = |reg: u8| -> (u8, bool) {
        if rex == 0 && (4..8).contains(&reg) {
            (reg - 4, true)
        } else {
            (reg, false)
        }
    };
    let immediate = |at: usize, size: u8| -> Result<u64, Error> {
        let mut value = 0u64;
        for k in 0..size as usize {
            value |= (byte(at + k)? as u64) << (8 * k);
        }
        Ok(value)
    };
    let finish = |length: usize, operation: Operation| -> Result<Instruction, Error> {
        if length > 15 {
            return Err(Error::TooLong);
        }
        Ok(Instruction {
            length: length as u8,
            operation,
        })
    };

    let opcode = byte(i)?;
    i += 1;
    match opcode {
        // MOV r/m8, r8
        0x88 => {
            let (reg, length) = modrm(i)?;
            let (reg, high) = byte_register(reg);
            let value = register(&regs.basic, reg) >> if high { 8 } else { 0 };
            finish(
                i + length,
                Operation::Store {
                    size: 1,
                    value: value & 0xFF,
                },
            )
        }
        // MOV r/m, r
        0x89 => {
            let (reg, length) = modrm(i)?;
            let value = register(&regs.basic, reg) & mask(operand_size);
            finish(
                i + length,
                Operation::Store {
                    size: operand_size,
                    value,
                },
            )
        }
        // MOV r8, r/m8
        0x8A => {
            let (reg, length) = modrm(i)?;
            let (register, high_byte) = byte_register(reg);
            finish(
                i + length,
                Operation::Load {
                    size: 1,
                    register,
                    high_byte,
                    dest_size: 1,
                    sign_extend: false,
                },
            )
        }
        // MOV r, r/m
        0x8B => {
            let (register, length) = modrm(i)?;
            finish(
                i + length,
                Operation::Load {
                    size: operand_size,
                    register,
                    high_byte: false,
                    dest_size: operand_size,
                    sign_extend: false,
                },
            )
        }
        // MOV r/m8, imm8
        0xC6 => {
            let (_, length) = modrm(i)?;
            let value = immediate(i + length, 1)?;
            finish(i + length + 1, Operation::Store { size: 1, value })
        }
        // MOV r/m, imm16/imm32 (sign-extended to 64 bits)
        0xC7 => {
            let (_, length) = modrm(i)?;
            let immediate_size = operand_size.min(4);
            let mut value = immediate(i + length, immediate_size)?;
            if operand_size == 8 {
                value = value as u32 as i32 as i64 as u64;
            }
            finish(
                i + length + immediate_size as usize,
                Operation::Store {
                    size: operand_size,
                    value,
                },
            )
        }
        // MOV AL/eAX, moffs and MOV moffs, AL/eAX.
        0xA0..=0xA3 => {
            let offset_size = if long_mode && !address32 { 8 } else { 4 };
            let size = if opcode & 1 == 0 { 1 } else { operand_size };
            let length = i + offset_size;
            if opcode < 0xA2 {
                finish(
                    length,
                    Operation::Load {
                        size,
                        register: 0,
                        high_byte: false,
                        dest_size: size,
                        sign_extend: false,
                    },
                )
            } else {
                finish(
                    length,
                    Operation::Store {
                        size,
                        value: regs.basic.rax & mask(size),
                    },
                )
            }
        }
        0x0F => {
            let second = byte(i)?;
            i += 1;
            match second {
                // MOVZX / MOVSX r, r/m8 / r/m16
                0xB6 | 0xB7 | 0xBE | 0xBF => {
                    let (register, length) = modrm(i)?;
                    finish(
                        i + length,
                        Operation::Load {
                            size: if second & 1 == 0 { 1 } else { 2 },
                            register,
                            high_byte: false,
                            dest_size: operand_size,
                            sign_extend: second >= 0xBE,
                        },
                    )
                }
                _ => Err(Error::Unsupported(vec![0x0F, second])),
            }
        }
        _ => Err(Error::Unsupported(vec![opcode])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Segment;
    use crate::x86::{CR4_PAE, PTE_WRITABLE};

    /// 64-bit code: EFER.LMA and CS.L.
    fn long_mode() -> Regs {
        Regs {
            basic: BasicRegs {
                rax: 0x1122_3344_5566_7788,
                ..Default::default()
            },
            cs: Segment {
                l: 1,
                ..Default::default()
            },
            efer: EFER_LMA,
            ..Default::default()
        }
    }

    fn decoded(bytes: &[u8]) -> Instruction {
        decode(bytes, &long_mode()).unwrap()
    }

    #[test]
    fn stores() {
        // mov [rdi], eax
        let i = decoded(&[0x89, 0x07]);
        assert_eq!(i.length, 2);
        assert!(matches!(
            i.operation,
            Operation::Store {
                size: 4,
                value: 0x5566_7788
            }
        ));
        // mov [rdi], rax
        let i = decoded(&[0x48, 0x89, 0x07]);
        assert!(matches!(
            i.operation,
            Operation::Store {
                size: 8,
                value: 0x1122_3344_5566_7788
            }
        ));
        // mov [rdi], ax
        let i = decoded(&[0x66, 0x89, 0x07]);
        assert!(matches!(
            i.operation,
            Operation::Store {
                size: 2,
                value: 0x7788
            }
        ));
        // mov dword [rdi + 0x10], 0x12345678
        let i = decoded(&[0xC7, 0x47, 0x10, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!(i.length, 7);
        assert!(matches!(
            i.operation,
            Operation::Store {
                size: 4,
                value: 0x1234_5678
            }
        ));
        // mov qword [rdi], -1 (sign-extended imm32)
        let i = decoded(&[0x48, 0xC7, 0x07, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(
            i.operation,
            Operation::Store {
                size: 8,
                value: u64::MAX
            }
        ));
        // mov [0xfec00000], eax (SIB without base, disp32)
        let i = decoded(&[0x89, 0x04, 0x25, 0x00, 0x00, 0xC0, 0xFE]);
        assert_eq!(i.length, 7);
    }

    #[test]
    fn loads() {
        // mov eax, [rdi]
        let i = decoded(&[0x8B, 0x07]);
        assert!(matches!(
            i.operation,
            Operation::Load {
                size: 4,
                register: 0,
                dest_size: 4,
                sign_extend: false,
                ..
            }
        ));
        // mov r8d, [rdi + 8]
        let i = decoded(&[0x44, 0x8B, 0x47, 0x08]);
        assert_eq!(i.length, 4);
        assert!(matches!(i.operation, Operation::Load { register: 8, .. }));
        // movzx eax, byte [rdi + 4]
        let i = decoded(&[0x0F, 0xB6, 0x47, 0x04]);
        assert_eq!(i.length, 4);
        assert!(matches!(
            i.operation,
            Operation::Load {
                size: 1,
                dest_size: 4,
                ..
            }
        ));
        // mov ah, [rdi + 1]
        let i = decoded(&[0x8A, 0x67, 0x01]);
        assert!(matches!(
            i.operation,
            Operation::Load {
                register: 0,
                high_byte: true,
                ..
            }
        ));
    }

    #[test]
    fn load_completion() {
        let mut regs = long_mode().basic;
        // 32-bit loads zero-extend.
        complete_load(&mut regs, &decoded(&[0x8B, 0x07]).operation, 0xAABB_CCDD);
        assert_eq!(regs.rax, 0xAABB_CCDD);
        // 8-bit loads merge.
        regs.rax = 0x1122_3344_5566_7788;
        complete_load(&mut regs, &decoded(&[0x8A, 0x07]).operation, 0xEE);
        assert_eq!(regs.rax, 0x1122_3344_5566_77EE);
        // AH.
        complete_load(&mut regs, &decoded(&[0x8A, 0x67, 0x01]).operation, 0x99);
        assert_eq!(regs.rax, 0x1122_3344_5566_99EE);
        // movsx eax, word [rdi]
        complete_load(&mut regs, &decoded(&[0x0F, 0xBF, 0x07]).operation, 0x8000);
        assert_eq!(regs.rax, 0xFFFF_8000);
    }

    #[test]
    fn fetch_through_page_tables() {
        let memory = GuestMemory::detached(0x10_0000);
        // PML4 at 0x1000, PDPT at 0x2000, PD at 0x3000 with a 2 MiB page at 0 whose
        // linear address is 0xFFFF_FFFF_8000_0000 (PML4 index 511, PDPT index 510).
        let flags = PTE_PRESENT | PTE_WRITABLE;
        memory.write_u64(0x1000 + 511 * 8, 0x2000 | flags).unwrap();
        memory.write_u64(0x2000 + 510 * 8, 0x3000 | flags).unwrap();
        memory.write_u64(0x3000, flags | PTE_LARGE).unwrap();
        // mov eax, [rdi] at the end of the first 4 KiB page: the fetch crosses a page.
        memory.write(0xFFF, &[0x8B, 0x07]).unwrap();
        let mut regs = long_mode();
        regs.cr0 = CR0_PG;
        regs.cr3 = 0x1000;
        regs.cr4 = CR4_PAE;
        regs.basic.rip = 0xFFFF_FFFF_8000_0FFF;
        let mut bytes = [0u8; 15];
        assert_eq!(fetch(&memory, &regs, &mut bytes), 15);
        assert_eq!(decode(&bytes, &regs).unwrap().length, 2);
        // Without paging, the linear address is the physical one.
        regs.cr0 = 0;
        regs.basic.rip = 0xFFFF_0000;
        assert_eq!(fetch(&memory, &regs, &mut bytes), 0);
        regs.basic.rip = 0xFFF;
        assert_eq!(fetch(&memory, &regs, &mut bytes), 15);
    }

    #[test]
    fn rejects_unsupported() {
        // rep movsd
        assert_eq!(decode(&[0xF3, 0xA5], &long_mode()).unwrap_err(), Error::Rep);
        // mov eax, eax
        assert_eq!(
            decode(&[0x89, 0xC0], &long_mode()).unwrap_err(),
            Error::RegisterOperand
        );
        assert_eq!(decode(&[0x89], &long_mode()).unwrap_err(), Error::Truncated);
        // mov eax, [bx] (16-bit addressing in 32-bit code)
        let protected = Regs {
            cs: Segment {
                db: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            decode(&[0x67, 0x8B, 0x07], &protected).unwrap_err(),
            Error::Address16
        );
        // cpuid
        assert_eq!(
            decode(&[0x0F, 0xA2], &long_mode()).unwrap_err(),
            Error::Unsupported(vec![0x0F, 0xA2])
        );
    }
}
