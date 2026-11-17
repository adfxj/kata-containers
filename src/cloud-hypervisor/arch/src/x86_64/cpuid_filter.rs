// Copyright © 2023 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use crate::x86_64::CpuidReg;
use hypervisor::arch::x86::{CpuIdCustomEntry, CpuIdEntry};
use log::{debug, warn};

/// Intel brand string.
pub const VENDOR_ID_INTEL: &[u8; 12] = b"GenuineIntel";

/// AMD brand string.
pub const VENDOR_ID_AMD: &[u8; 12] = b"AuthenticAMD";

pub fn apply_compatible_template(cpuids: &mut [CpuIdEntry]) {
    if let Some(vendor) = get_vendor_id_from_host() {
        if *VENDOR_ID_INTEL == vendor {
            debug!("Got Intel machine");
            apply_template(cpuids, &t2cl());
        } else if *VENDOR_ID_AMD == vendor {
            debug!("Got AMD machine");
            apply_template(cpuids, &t2a());
        } else {
            warn!("Unsupported vendor");
        }
    }
}

/// Extracts the CPU vendor id from leaf 0x0.
///
/// # Errors
///
/// When CPUID leaf 0 is not supported.
pub fn get_vendor_id_from_host() -> Option<[u8; 12]> {
    // SAFETY: call cpuid with valid leaves
    unsafe {
        let entry = std::arch::x86_64::__cpuid(0);
        // The ordering of the vendor string is ebx,edx,ecx this is not a mistake.
        Some(std::mem::transmute::<[u32; 3], [u8; 12]>([
            entry.ebx, entry.edx, entry.ecx,
        ]))
    }
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct CustomCpuId {
    pub cpuids: Vec<CpuIdCustomEntry>,
}

/// Target register to be modified by a bitmap.
#[derive(Debug, Clone)]
pub struct CpuidRegisterModifier {
    /// CPUID register to be modified by the bitmap.
    pub register: CpuidReg,
    /// Bit mapping to be applied as a modifier to the
    /// register's value at the address provided.
    pub bitmap: RegisterValueFilter,
}

/// Composite type that holistically provides
/// the location of a specific register being used
/// in the context of a CPUID tree.
#[derive(Debug, Default, Clone)]
pub struct CpuidLeafModifier {
    /// Leaf value.
    pub leaf: u32,
    /// Sub-Leaf value.
    pub subleaf: u32,
    /// KVM feature flags for this leaf-subleaf.
    pub flags: u32,
    /// All registers to be modified under the sub-leaf.
    pub modifiers: Vec<CpuidRegisterModifier>,
}

/// Bit-mapped value to adjust targeted bits of a register.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
pub struct RegisterValueFilter {
    /// Filter to be used when writing the value bits.
    pub filter: u32,
    /// Value to be applied.
    pub value: u32,
}

impl RegisterValueFilter {
    /// Applies filter to the value
    #[inline]
    pub fn apply(&self, value: u32) -> u32 {
        (value & !self.filter) | self.value
    }
}

/// Wrapper type to containing x86_64 CPU config modifiers.
#[derive(Debug, Default, Clone)]
pub struct CustomCpuTemplate {
    /// Modifiers for CPUID configuration.
    pub cpuid_modifiers: Vec<CpuidLeafModifier>,
}

pub fn apply_template(cpuids: &mut [CpuIdEntry], template: &CustomCpuTemplate) {
    // Apply CPUID modifiers
    for mod_leaf in template.cpuid_modifiers.iter() {
        let function = mod_leaf.leaf;
        let index = mod_leaf.subleaf;

        for entry in &mut *cpuids {
            if entry.function == function && entry.index == index {
                entry.flags = mod_leaf.flags;

                for mod_reg in &mod_leaf.modifiers {
                    match mod_reg.register {
                        CpuidReg::EAX => {
                            entry.eax = mod_reg.bitmap.apply(entry.eax);
                        }
                        CpuidReg::EBX => {
                            entry.ebx = mod_reg.bitmap.apply(entry.ebx);
                        }
                        CpuidReg::ECX => {
                            entry.ecx = mod_reg.bitmap.apply(entry.ecx);
                        }
                        CpuidReg::EDX => {
                            entry.edx = mod_reg.bitmap.apply(entry.edx);
                        }
                    }
                }
            }
        }
    }
}

/// T2CL template
///
/// Mask CPUID to make exposed CPU features as close as possbile to Intel Cascade Lake and provide
/// instruction set feature partity with AMD Milan using T2A template.
///
/// References:
/// - Intel SDM: https://cdrdv2.intel.com/v1/dl/getContent/671200
/// - AMD APM: https://www.amd.com/system/files/TechDocs/40332.pdf
/// - CPUID Enumeration and Architectural MSRs: https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/cpuid-enumeration-and-architectural-msrs.html
#[allow(clippy::unusual_byte_groupings)]
pub fn t2cl() -> CustomCpuTemplate {
    CustomCpuTemplate {
        cpuid_modifiers: vec![
            CpuidLeafModifier {
                leaf: 0x1,
                subleaf: 0x0,
                flags: 0,
                modifiers: vec![
                    // EAX: Version Information
                    CpuidRegisterModifier {
                        register: CpuidReg::EAX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_11111111_1111_00_11_1111_1111_1111,
                            value: 0b0000_00000000_0011_00_00_0110_1111_0010,
                        },
                    },
                    // ECX: Feature Information - clear Intel-only bits
                    CpuidRegisterModifier {
                        register: CpuidReg::ECX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0100_1100_1101_1111_1100,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                    // EDX: Feature Information
                    CpuidRegisterModifier {
                        register: CpuidReg::EDX,
                        bitmap: RegisterValueFilter {
                            filter: 0b1110_1000_0110_0100_0001_0000_1000_0000,
                            value: 0b0000_0000_0000_0000_0001_0000_1000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0x7,
                subleaf: 0x0,
                flags: 1,
                modifiers: vec![
                    // EBX: clear Intel-only features
                    CpuidRegisterModifier {
                        register: CpuidReg::EBX,
                        bitmap: RegisterValueFilter {
                            filter: 0b1111_1111_1110_1111_1101_1010_0001_0100,
                            value: 0b0000_0000_0000_0000_0000_0010_0000_0000,
                        },
                    },
                    // ECX: clear Intel-only AVX512 features
                    CpuidRegisterModifier {
                        register: CpuidReg::ECX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0100_0000_0100_0001_0101_1111_0101_1110,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                    // EDX: clear Intel-only AVX512 features
                    CpuidRegisterModifier {
                        register: CpuidReg::EDX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_0001_0001_1100,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0xd,
                subleaf: 0x0,
                flags: 1,
                modifiers: vec![
                    // EAX: clear MPX, AVX-512, PKRU state bits
                    CpuidRegisterModifier {
                        register: CpuidReg::EAX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_00_1_0_111_11_000,
                            value: 0b0000_0000_0000_0000_0000_00_0_0_000_00_000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0xd,
                subleaf: 0x1,
                flags: 1,
                modifiers: vec![
                    // EAX: clear XSAVEC, XGETBV, XSAVES
                    CpuidRegisterModifier {
                        register: CpuidReg::EAX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_0000_0000_1110,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0x80000001,
                subleaf: 0x0,
                flags: 0,
                modifiers: vec![
                    // ECX: clear AMD-only features (SSE4A, MisAlignSse, 3DNowPrefetch, MONITORX)
                    CpuidRegisterModifier {
                        register: CpuidReg::ECX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0010_0000_0000_0000_0000_0001_1100_0000,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                    // EDX: clear AMD-only features (MmxExt, MMX, FSXR, FFXSR, Page1GB)
                    CpuidRegisterModifier {
                        register: CpuidReg::EDX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0111_1100_0000_0000_0000_0000_0000,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0x80000008,
                subleaf: 0x0,
                flags: 0,
                modifiers: vec![
                    // EBX: clear WBNOINVD
                    CpuidRegisterModifier {
                        register: CpuidReg::EBX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_0010_0000_0000,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
        ],
    }
}

/// T2A template
///
/// Provide instruction set feature partity with Intel Cascade Lake or later using T2CL template.
///
/// References:
/// - Intel SDM: https://cdrdv2.intel.com/v1/dl/getContent/671200
/// - AMD APM: https://www.amd.com/system/files/TechDocs/40332.pdf
/// - CPUID Enumeration and Architectural MSRs: https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/cpuid-enumeration-and-architectural-msrs.html
#[allow(clippy::unusual_byte_groupings)]
pub fn t2a() -> CustomCpuTemplate {
    CustomCpuTemplate {
        cpuid_modifiers: vec![
            CpuidLeafModifier {
                leaf: 0x1,
                subleaf: 0x0,
                flags: 0,
                modifiers: vec![
                    // EAX: Version Information
                    CpuidRegisterModifier {
                        register: CpuidReg::EAX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_11111111_1111_00_11_1111_1111_1111,
                            value: 0b0000_00000000_0011_00_00_0110_1111_0010,
                        },
                    },
                    // ECX: Feature Information - clear Intel-only bits
                    CpuidRegisterModifier {
                        register: CpuidReg::ECX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0100_1100_1101_1111_1100,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                    // EDX: Feature Information
                    CpuidRegisterModifier {
                        register: CpuidReg::EDX,
                        bitmap: RegisterValueFilter {
                            filter: 0b1110_1000_0110_0100_0001_0000_1000_0000,
                            value: 0b0000_0000_0000_0000_0001_0000_1000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0x7,
                subleaf: 0x0,
                flags: 1,
                modifiers: vec![
                    // EBX: clear Intel-only features
                    CpuidRegisterModifier {
                        register: CpuidReg::EBX,
                        bitmap: RegisterValueFilter {
                            filter: 0b1111_1111_1110_1111_1101_1010_0001_0100,
                            value: 0b0000_0000_0000_0000_0000_0010_0000_0000,
                        },
                    },
                    // ECX: clear Intel-only AVX512 features
                    CpuidRegisterModifier {
                        register: CpuidReg::ECX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0100_0000_0100_0001_0101_1111_0101_1110,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                    // EDX: clear Intel-only AVX512 features
                    CpuidRegisterModifier {
                        register: CpuidReg::EDX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_0001_0001_1100,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0xd,
                subleaf: 0x0,
                flags: 1,
                modifiers: vec![
                    // EAX: clear MPX, AVX-512, PKRU state bits
                    CpuidRegisterModifier {
                        register: CpuidReg::EAX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_00_1_0_111_11_000,
                            value: 0b0000_0000_0000_0000_0000_00_0_0_000_00_000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0xd,
                subleaf: 0x1,
                flags: 1,
                modifiers: vec![
                    // EAX: clear XSAVEC, XGETBV, XSAVES
                    CpuidRegisterModifier {
                        register: CpuidReg::EAX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0000_0000_0000_0000_0000_1110,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0x80000001,
                subleaf: 0x0,
                flags: 0,
                modifiers: vec![
                    // ECX: clear AMD-only features including SVM
                    CpuidRegisterModifier {
                        register: CpuidReg::ECX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0010_0000_0000_0000_0000_0001_1100_0100,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                    // EDX: clear AMD-only features (MmxExt, FFXSR, Page1GB)
                    CpuidRegisterModifier {
                        register: CpuidReg::EDX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0110_0100_0000_0000_0000_0000_0000,
                            value: 0b0000_0000_0000_0000_0000_0000_0000_0000,
                        },
                    },
                ],
            },
            CpuidLeafModifier {
                leaf: 0x80000008,
                subleaf: 0x0,
                flags: 0,
                modifiers: vec![
                    // EBX: clear AMD-specific features
                    CpuidRegisterModifier {
                        register: CpuidReg::EBX,
                        bitmap: RegisterValueFilter {
                            filter: 0b0000_0000_0001_1100_0000_0010_0000_0101,
                            value: 0b0000_0000_0001_1100_0000_0000_0000_0100,
                        },
                    },
                ],
            },
        ],
    }
}
