//! WCH-LinkRV probe support.
//!
//! The protocol is mostly undocumented, and is changing between firmware versions.
//! For more details see: <https://github.com/ch32-rs/wlink>

use std::fmt;
use std::time::Duration;

use bitvec::{bitvec, field::BitField, order::Lsb0, vec::BitVec, view::BitView};
use nusb::DeviceInfo;
use probe_rs_target::ScanChainElement;

use self::{commands::Speed, usb_interface::WchLinkUsbDevice};
use super::JtagAccess;
use crate::{
    architecture::riscv::{
        communication_interface::{RiscvError, RiscvInterfaceBuilder},
        dtm::jtag_dtm::JtagDtmBuilder,
    },
    probe::{
        DebugProbe, DebugProbeError, DebugProbeInfo, DebugProbeKind, DebugProbeSelector,
        JtagSequence, ProbeError, ProbeFactory, UsbFilters, WireProtocol,
    },
};

mod commands;
mod usb_interface;

const VENDOR_ID: u16 = 0x1a86;
const PRODUCT_ID: u16 = 0x8010;

// See: RISC-V Debug Specification, 6.1 JTAG DTM Registers
const DMI_VALUE_BIT_OFFSET: u32 = 2;
const DMI_ADDRESS_BIT_OFFSET: u32 = 34;
const DMI_OP_MASK: u128 = 0b11; // 2 bits

const DMI_OP_NOP: u8 = 0;
const DMI_OP_READ: u8 = 1;
const DMI_OP_WRITE: u8 = 2;

const REG_BYPASS_ADDRESS: u8 = 0x1f;
const REG_IDCODE_ADDRESS: u8 = 0x01;
const REG_DTMCS_ADDRESS: u8 = 0x10;
const REG_DMI_ADDRESS: u8 = 0x11;

const DTMCS_DMIRESET_MASK: u32 = 1 << 16;
const DTMCS_DMIHARDRESET_MASK: u32 = 1 << 17;

/// All WCH-Link probe variants, see-also: <http://www.wch-ic.com/products/WCH-Link.html>
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WchLinkVariant {
    /// WCH-Link-CH549, does not support RV32EC
    Ch549 = 1,
    /// WCH-LinkE-CH32V305, the full featured version
    ECh32v305 = 2,
    /// WCH-LinkS-CH32V203
    SCh32v203 = 3,
    /// WCH-LinkW-CH32V208, a wirelessed version
    WCh32v208 = 5,
}

impl fmt::Display for WchLinkVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WchLinkVariant::Ch549 => write!(f, "WCH-Link-CH549"),
            WchLinkVariant::ECh32v305 => write!(f, "WCH-LinkE-CH32V305"),
            WchLinkVariant::SCh32v203 => write!(f, "WCH-LinkS-CH32V203"),
            WchLinkVariant::WCh32v208 => write!(f, "WCH-LinkW-CH32V208"),
        }
    }
}

impl WchLinkVariant {
    fn try_from_u8(value: u8) -> Result<Self, WchLinkError> {
        match value {
            1 => Ok(Self::Ch549),
            2 | 0x12 => Ok(Self::ECh32v305),
            3 => Ok(Self::SCh32v203),
            5 | 0x85 => Ok(Self::WCh32v208),
            _ => Err(WchLinkError::UnknownDevice),
        }
    }
}

/// Currently supported RISC-V chip series/families. The IP core name is "Qingke".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RiscvChip {
    /// CH32V103 Qingke-V3A series
    CH32V103 = 0x01,
    /// CH571/CH573 Qingke-V3A BLE 4.2 series
    CH57X = 0x02,
    /// CH565/CH569 Qingke-V3A series
    CH56X = 0x03,
    /// CH32V20X Qingke-V4B/V4C series
    CH32V20X = 0x05,
    /// CH32V30X Qingke-V4C/V4F series, the same as CH32V20X
    CH32V30X = 0x06,
    /// CH58x Qingke-V4A BLE 5.3 series
    CH58X = 0x07,
    /// CH32V003 Qingke-V2A series
    CH32V003 = 0x09,
    // The only reference I can find is <https://www.wch.cn/news/606.html>.
    /// RISC-V EC controller, undocumented.
    CH8571 = 0x0A, // 10,
    /// CH59x Qingke-V4C BLE 5.4 series, fallback as CH58X
    CH59X = 0x0B, // 11
    /// CH643 Qingke-V4C series, RGB Display Driver MCU
    CH643 = 0x0C, // 12
    /// CH32X035 Qingke-V4C USB-PD series, fallback as CH643
    CH32X035 = 0x0D, // 13
    /// CH32L103 Qingke-V4C low power series, USB-PD
    CH32L103 = 0x0E, // 14
    /// CH641 Qingke-V2A series, USB-PD, fallback as CH32V003
    CH641 = 0x49,
}

impl RiscvChip {
    fn try_from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(RiscvChip::CH32V103),
            0x02 => Some(RiscvChip::CH57X),
            0x03 => Some(RiscvChip::CH56X),
            0x05 => Some(RiscvChip::CH32V20X),
            0x06 => Some(RiscvChip::CH32V30X),
            0x07 => Some(RiscvChip::CH58X),
            0x09 => Some(RiscvChip::CH32V003),
            0x0A => Some(RiscvChip::CH8571),
            0x0B => Some(RiscvChip::CH59X),
            0x0C => Some(RiscvChip::CH643),
            0x0D => Some(RiscvChip::CH32X035),
            0x0E => Some(RiscvChip::CH32L103),
            0x49 => Some(RiscvChip::CH641),
            _ => None,
        }
    }

    fn support_flash_protect(&self) -> bool {
        matches!(
            self,
            RiscvChip::CH32V103
                | RiscvChip::CH32V20X
                | RiscvChip::CH32V30X
                | RiscvChip::CH32V003
                | RiscvChip::CH643
                | RiscvChip::CH32L103
                | RiscvChip::CH32X035
                | RiscvChip::CH641
        )
    }
}

/// Factory for creating [`WchLink`] probes.
#[derive(Debug)]
pub struct WchLinkFactory;

impl std::fmt::Display for WchLinkFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WchLink")
    }
}

impl ProbeFactory for WchLinkFactory {
    fn open(&self, selector: &DebugProbeSelector) -> Result<Box<dyn DebugProbe>, DebugProbeError> {
        let device = WchLinkUsbDevice::new_from_selector(selector)?;
        let mut wlink = WchLink {
            device,
            name: "WCH-Link".into(),
            variant: WchLinkVariant::Ch549,
            v_major: 0,
            v_minor: 0,
            chip_id: 0,
            chip_family: RiscvChip::CH32V103,
            last_dmi_read: None,
            speed: Speed::default(),
            idle_cycles: 0,
        };

        wlink.init()?;

        Ok(Box::new(wlink))
    }

    fn list_probes(&self) -> Vec<DebugProbeInfo> {
        list_wlink_devices()
    }
}

/// A WCH-Link device (mod:RV)
pub struct WchLink {
    device: WchLinkUsbDevice,
    name: String,
    variant: WchLinkVariant,
    v_major: u8,
    v_minor: u8,
    /// Chip family
    chip_family: RiscvChip,
    /// Chip id to identify the target chip variant
    chip_id: u32,
    // Hack to support NOP after READ
    last_dmi_read: Option<(u8, u32, u8)>,
    speed: commands::Speed,
    idle_cycles: u8,
}

impl fmt::Debug for WchLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WchLink")
            .field("name", &self.name)
            .field("variant", &self.variant)
            .field("v_major", &self.v_major)
            .field("v_minor", &self.v_minor)
            .field("chip_family", &self.chip_family)
            .field("chip_id", &self.chip_id)
            .field("last_dmi_read", &self.last_dmi_read)
            .field("speed", &self.speed)
            .field("idle_cycles", &self.idle_cycles)
            .finish()
    }
}

impl WchLink {
    fn get_probe_info(&mut self) -> Result<(), DebugProbeError> {
        let probe_info = self.device.send_command(commands::GetProbeInfo)?;
        self.v_major = probe_info.major_version;
        self.v_minor = probe_info.minor_version;

        if self.v_major != 0x02 && self.v_minor < 0x07 {
            return Err(WchLinkError::UnsupportedFirmwareVersion("2.7").into());
        }

        self.variant = probe_info.variant;

        Ok(())
    }

    fn init(&mut self) -> Result<(), DebugProbeError> {
        // first stage of wlink_init
        tracing::debug!("Initializing WCH-Link...");

        self.get_probe_info()?;

        // this is the official version format. So "v31" is actually a 2.11
        let version_code = self.v_major * 10 + self.v_minor;

        tracing::info!(
            "WCH-Link variant: {}, firmware version: {}.{} (v{})",
            self.variant,
            self.v_major,
            self.v_minor,
            version_code
        );

        if self.v_major != 0x02 && self.v_minor < 0x7 {
            return Err(WchLinkError::UnsupportedFirmwareVersion("2.7").into());
        }
        self.name = format!("{} v{}.{}", self.variant, self.v_major, self.v_minor);

        Ok(())
    }

    fn dmi_op_read(&mut self, addr: u8) -> Result<(u8, u32, u8), DebugProbeError> {
        let resp = self.device.send_command(commands::DmiOp::read(addr))?;

        Ok((resp.addr, resp.data, resp.op))
    }

    fn dmi_op_write(&mut self, addr: u8, data: u32) -> Result<(u8, u32, u8), DebugProbeError> {
        let resp = self
            .device
            .send_command(commands::DmiOp::write(addr, data))?;

        Ok((resp.addr, resp.data, resp.op))
    }

    fn dmi_op_nop(&mut self) -> Result<(u8, u32, u8), DebugProbeError> {
        let resp = self.device.send_command(commands::DmiOp::nop())?;

        Ok((resp.addr, resp.data, resp.op))
    }
}

impl DebugProbe for WchLink {
    fn get_name(&self) -> &str {
        &self.name
    }

    fn speed_khz(&self) -> u32 {
        self.speed.to_khz()
    }

    fn set_speed(&mut self, speed_khz: u32) -> Result<u32, DebugProbeError> {
        let speed =
            Speed::from_khz(speed_khz).ok_or(DebugProbeError::UnsupportedSpeed(speed_khz))?;
        self.speed = speed;
        self.device
            .send_command(commands::SetSpeed(self.chip_family, speed))?;
        Ok(speed.to_khz())
    }

    /// Attach chip
    fn attach(&mut self) -> Result<(), DebugProbeError> {
        // second stage of wlink_init
        tracing::trace!("attach to target chip");

        self.device
            .send_command(commands::SetSpeed(self.chip_family, self.speed))?;

        let resp = self.device.send_command(commands::AttachChip)?;

        self.chip_family = resp.chip_family;

        tracing::info!("attached riscv chip {:?}", self.chip_family);

        self.chip_id = resp.chip_id;

        if self.chip_family.support_flash_protect() {
            self.device.send_command(commands::CheckFlashProtection)?;
            self.device.send_command(commands::UnprotectFlash)?;
        }

        Ok(())
    }

    fn detach(&mut self) -> Result<(), crate::Error> {
        tracing::trace!("Detach chip");
        self.device.send_command(commands::DetachChip)?;

        Ok(())
    }

    fn target_reset(&mut self) -> Result<(), DebugProbeError> {
        self.device.send_command(commands::ResetTarget)?;
        Ok(())
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        tracing::info!("target reset assert");
        self.device
            .send_command(commands::DmiOp::write(0x10, 0x80000001))?;
        Ok(())
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        tracing::info!("target reset deassert");
        self.device
            .send_command(commands::DmiOp::write(0x10, 0x00000001))?;
        Ok(())
    }

    fn select_protocol(&mut self, protocol: WireProtocol) -> Result<(), DebugProbeError> {
        // Assume Jtag, as it is the only supported protocol for riscv
        match protocol {
            WireProtocol::Jtag => Ok(()),
            _ => Err(DebugProbeError::UnsupportedProtocol(protocol)),
        }
    }

    fn active_protocol(&self) -> Option<WireProtocol> {
        Some(WireProtocol::Jtag)
    }

    fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
        self
    }

    fn has_riscv_interface(&self) -> bool {
        true
    }

    fn try_get_riscv_interface_builder<'probe>(
        &'probe mut self,
    ) -> Result<Box<dyn RiscvInterfaceBuilder<'probe> + 'probe>, RiscvError> {
        Ok(Box::new(JtagDtmBuilder::new(self)))
    }
}

/// Wrap WCH-Link's USB based DMI access as a fake JtagAccess
impl JtagAccess for WchLink {
    fn set_scan_chain(&mut self, _scan_chain: &[ScanChainElement]) -> Result<(), DebugProbeError> {
        Ok(())
    }

    fn scan_chain(&mut self) -> Result<&[ScanChainElement], DebugProbeError> {
        Ok(&[])
    }

    fn tap_reset(&mut self) -> Result<(), DebugProbeError> {
        Ok(())
    }

    fn read_register(&mut self, address: u32, len: u32) -> Result<BitVec, DebugProbeError> {
        tracing::debug!("read register 0x{:08x}", address);
        assert_eq!(len, 32);

        let mut ret = bitvec![0; len as usize];
        match address as u8 {
            REG_IDCODE_ADDRESS => {
                // using hard coded idcode 0x00000001, the same as WCH's openocd fork
                tracing::debug!("using hard coded idcode 0x00000001");
                ret[0..8].store_le::<u8>(0x1);
                Ok(ret)
            }
            REG_DTMCS_ADDRESS => {
                // See: RISC-V Debug Specification, 6.1.4
                // 0x71: abits=7, version=1(1.0)
                ret[0..8].store_le::<u8>(0x71);
                Ok(ret)
            }
            REG_BYPASS_ADDRESS => Ok(bitvec![0; 4]),
            _ => panic!("unknown read register address {address:08x}"),
        }
    }

    fn set_idle_cycles(&mut self, idle_cycles: u8) -> Result<(), DebugProbeError> {
        self.idle_cycles = idle_cycles;
        Ok(())
    }

    fn idle_cycles(&self) -> u8 {
        self.idle_cycles
    }

    fn write_register(
        &mut self,
        address: u32,
        data: &[u8],
        len: u32,
    ) -> Result<BitVec, DebugProbeError> {
        match address as u8 {
            REG_DTMCS_ADDRESS => {
                let val = u32::from_le_bytes(data.try_into().unwrap());
                if val & DTMCS_DMIRESET_MASK != 0 {
                    tracing::debug!("DMI reset");
                    self.dmi_op_write(0x10, 0x00000000)?;
                    self.dmi_op_write(0x10, 0x00000001)?;
                    // dmcontrol.dmactive is checked later
                } else if val & DTMCS_DMIHARDRESET_MASK != 0 {
                    return Err(WchLinkError::UnsupportedOperation.into());
                }

                let mut ret = bitvec![0; len as usize];
                ret[0..8].store_le::<u8>(0x71);
                Ok(ret)
            }
            REG_DMI_ADDRESS => {
                assert_eq!(
                    len, 41,
                    "should be 41 bits: 8 bits abits + 32 bits data + 2 bits op"
                );
                let register_value: u128 = u128::from_le_bytes(data.try_into().unwrap());

                let dmi_addr = ((register_value >> DMI_ADDRESS_BIT_OFFSET) & 0x3f) as u8;
                let dmi_value = ((register_value >> DMI_VALUE_BIT_OFFSET) & 0xffffffff) as u32;
                let dmi_op = (register_value & DMI_OP_MASK) as u8;

                tracing::trace!(
                    "dmi op={} addr 0x{:02x} data 0x{:08x}",
                    dmi_op,
                    dmi_addr,
                    dmi_value,
                );

                let (addr, data, op) = match dmi_op {
                    DMI_OP_READ => {
                        let (addr, data, op) = self.dmi_op_read(dmi_addr)?;
                        tracing::trace!("dmi read 0x{:02x} 0x{:08x} op={}", addr, data, op);
                        self.last_dmi_read = Some((addr, data, op));
                        (addr, data, op)
                    }
                    DMI_OP_NOP => {
                        // No idea why NOP with zero addr should return the last read value.
                        // see-also: RiscvCommunicationInterface::read_dm_register_untyped
                        let (addr, data, op) = if dmi_addr == 0 && dmi_value == 0 {
                            self.last_dmi_read.unwrap()
                        } else {
                            self.dmi_op_nop()?
                        };
                        tracing::trace!("dmi nop 0x{:02x} 0x{:08x} op={}", addr, data, op);
                        (addr, data, op)
                    }
                    DMI_OP_WRITE => {
                        let (addr, data, op) = self.dmi_op_write(dmi_addr, dmi_value)?;
                        tracing::trace!("dmi write 0x{:02x} 0x{:08x} op={}", addr, data, op);
                        if dmi_addr == 0x10 && dmi_value == 0x40000001 {
                            // needs additional sleep for a resume operation
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        (addr, data, op)
                    }
                    _ => unreachable!("unknown dmi_op {dmi_op}"),
                };

                let ret = ((addr as u128) << DMI_ADDRESS_BIT_OFFSET)
                    | ((data as u128) << DMI_VALUE_BIT_OFFSET)
                    | (op as u128);

                let ret_bytes = ret.to_le_bytes();
                Ok(ret_bytes
                    .iter()
                    .fold(BitVec::with_capacity(128), |mut acc, s| {
                        acc.extend_from_bitslice(s.view_bits::<Lsb0>());
                        acc
                    }))
            }
            _ => unreachable!("unknown register address 0x{:08x}", address),
        }
    }

    fn write_dr(&mut self, _data: &[u8], _len: u32) -> Result<BitVec, DebugProbeError> {
        Err(DebugProbeError::NotImplemented {
            function_name: "write_dr",
        })
    }

    fn shift_raw_sequence(&mut self, _sequence: JtagSequence) -> Result<BitVec, DebugProbeError> {
        Err(DebugProbeError::NotImplemented {
            function_name: "shift_raw_sequence ",
        })
    }
}

fn get_wlink_info(device: &DeviceInfo) -> Option<DebugProbeInfo> {
    if matches!(device.product_string(), Some("WCH-Link") | Some("WCH_Link")) {
        Some(DebugProbeInfo::new(
            "WCH-Link",
            DebugProbeKind::Usb {
                vendor_id: VENDOR_ID,
                product_id: PRODUCT_ID,
                filters: UsbFilters {
                    serial_number: device.serial_number().map(str::to_string),
                    hid_interface: None,

                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    sysfs_path: Some(device.sysfs_path().to_owned()),

                    #[cfg(target_os = "windows")]
                    instance_id: Some(device.instance_id().display().to_string()),
                    #[cfg(target_os = "windows")]
                    parent_instance_id: Some(device.parent_instance_id().display().to_string()),
                    #[cfg(target_os = "windows")]
                    port_number: Some(device.port_number()),
                    #[cfg(target_os = "windows")]
                    driver: device.driver().map(str::to_string),

                    #[cfg(target_os = "macos")]
                    registry_id: Some(device.registry_entry_id()),
                    #[cfg(target_os = "macos")]
                    location_id: Some(device.location_id()),
                },
            },
            &WchLinkFactory,
        ))
    } else {
        None
    }
}

#[tracing::instrument(skip_all)]
fn list_wlink_devices() -> Vec<DebugProbeInfo> {
    tracing::debug!("Searching for WCH-Link(RV) probes");
    let Ok(devices) = nusb::list_devices() else {
        return vec![];
    };
    let probes: Vec<_> = devices
        .filter(|device| device.vendor_id() == VENDOR_ID && device.product_id() == PRODUCT_ID)
        .filter_map(|device| get_wlink_info(&device))
        .collect();

    tracing::debug!("Found {} WCH-Link probes total", probes.len());
    probes
}

#[derive(thiserror::Error, Debug, docsplay::Display)]
pub(crate) enum WchLinkError {
    /// Unknown WCH-Link device.
    UnknownDevice,
    /// The firmware on the probe is outdated, and not supported by probe-rs. The minimum supported firmware version is {0}.
    UnsupportedFirmwareVersion(&'static str),
    /// Not enough bytes written.
    NotEnoughBytesWritten { is: usize, should: usize },
    /// Not enough bytes read.
    NotEnoughBytesRead { is: usize, should: usize },
    /// Usb endpoint not found.
    EndpointNotFound,
    /// Invalid payload.
    InvalidPayload,
    /// Protocol error.
    Protocol(u8, Vec<u8>),
    /// Unknown chip {0:#04x}.
    UnknownChip(u8),
    /// Unsupported operation.
    UnsupportedOperation,
}

impl ProbeError for WchLinkError {}
