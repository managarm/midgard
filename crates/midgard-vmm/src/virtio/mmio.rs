//! The MMIO transport (version 2): each device's registers are in a page of their own.
//! Linux finds the devices through `virtio_mmio.device=` on its command line.

use super::{Effect, Half, Register, Transport};

/// The register window of one device.
pub(crate) const MMIO_SIZE: u64 = 0x1000;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const VENDOR: u32 = 0x4D47_524D; // "MRGM"

// Register offsets.
const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00C;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090;
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0A0;
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0A4;
const REG_SHM_LEN_LOW: u64 = 0x0B0;
const REG_SHM_LEN_HIGH: u64 = 0x0B4;
const REG_SHM_BASE_LOW: u64 = 0x0B8;
const REG_SHM_BASE_HIGH: u64 = 0x0BC;
const REG_CONFIG_GENERATION: u64 = 0x0FC;
const REG_CONFIG: u64 = 0x100;

impl Transport {
    /// Reads `size` bytes of the registers at `offset`.
    pub(crate) fn read(&self, offset: u64, size: u8) -> u64 {
        self.counters.registers.reads.add();
        if offset >= REG_CONFIG {
            return self.read_config(offset - REG_CONFIG, size);
        }
        let register = match offset {
            REG_MAGIC => return MAGIC.into(),
            REG_VERSION => return VERSION.into(),
            REG_DEVICE_ID => return self.device_id().into(),
            REG_VENDOR_ID => return VENDOR.into(),
            REG_INTERRUPT_STATUS => return self.interrupt_status().into(),
            // No shared memory regions: a length of all ones means "absent".
            REG_SHM_LEN_LOW | REG_SHM_LEN_HIGH | REG_SHM_BASE_LOW | REG_SHM_BASE_HIGH => {
                return u32::MAX.into();
            }
            REG_DEVICE_FEATURES => Register::DeviceFeatures,
            REG_QUEUE_NUM_MAX => Register::QueueSizeMax,
            REG_QUEUE_READY => Register::QueueReady,
            REG_STATUS => Register::Status,
            REG_CONFIG_GENERATION => Register::ConfigGeneration,
            _ => return 0,
        };
        self.read_register(register).into()
    }

    /// Writes `size` bytes of the registers at `offset`.
    pub(crate) fn write(&self, offset: u64, size: u8, value: u64) -> Effect {
        self.counters.registers.writes.add();
        if offset >= REG_CONFIG {
            self.write_config(offset - REG_CONFIG, size, value);
            return Effect::None;
        }
        let value = value as u32;
        let register = match offset {
            REG_QUEUE_NOTIFY => {
                self.notify(value & 0xFFFF);
                return Effect::None;
            }
            REG_INTERRUPT_ACK => {
                self.acknowledge_interrupt(value);
                return Effect::None;
            }
            REG_DEVICE_FEATURES_SEL => Register::DeviceFeaturesSel,
            REG_DRIVER_FEATURES => Register::DriverFeatures,
            REG_DRIVER_FEATURES_SEL => Register::DriverFeaturesSel,
            REG_QUEUE_SEL => Register::QueueSel,
            REG_QUEUE_NUM => Register::QueueSize,
            REG_QUEUE_READY => Register::QueueReady,
            REG_QUEUE_DESC_LOW => Register::QueueDesc(Half::Low),
            REG_QUEUE_DESC_HIGH => Register::QueueDesc(Half::High),
            REG_QUEUE_DRIVER_LOW => Register::QueueDriver(Half::Low),
            REG_QUEUE_DRIVER_HIGH => Register::QueueDriver(Half::High),
            REG_QUEUE_DEVICE_LOW => Register::QueueDevice(Half::Low),
            REG_QUEUE_DEVICE_HIGH => Register::QueueDevice(Half::High),
            REG_STATUS => Register::Status,
            _ => return Effect::None,
        };
        self.write_register(register, value)
    }
}
