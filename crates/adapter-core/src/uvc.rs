//! USB Video Class control plumbing that needs no hardware: finding the
//! Video Control interface and its Camera Terminal and Processing Unit in a
//! configuration descriptor, the table of controls the HTTP query parameters
//! map to, and encoding/decoding of their values. The firmware only adds the
//! control transfers.

/// Video Control interface and the entities we send requests to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VcInfo {
    /// `bInterfaceNumber` of the VideoControl interface (wIndex low byte).
    pub interface: u8,
    pub camera_terminal: Option<Entity>,
    pub processing_unit: Option<Entity>,
}

/// A terminal or unit: its ID (wIndex high byte) and `bmControls` bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entity {
    pub id: u8,
    pub controls: u32,
}

const DT_INTERFACE: u8 = 0x04;
const DT_CS_INTERFACE: u8 = 0x24;
const CLASS_VIDEO: u8 = 0x0e;
const SUBCLASS_VIDEOCONTROL: u8 = 0x01;
const VC_INPUT_TERMINAL: u8 = 0x02;
const VC_PROCESSING_UNIT: u8 = 0x05;
const ITT_CAMERA: u16 = 0x0201;

/// Walks a full configuration descriptor and returns the first VideoControl
/// interface with its camera terminal and processing unit.
pub fn parse_vc(cfg: &[u8]) -> Option<VcInfo> {
    let mut info: Option<VcInfo> = None;
    let mut in_vc = false;
    let mut pos = 0;
    while pos + 2 <= cfg.len() {
        let len = cfg[pos] as usize;
        if len < 2 || pos + len > cfg.len() {
            break;
        }
        let d = &cfg[pos..pos + len];
        match d[1] {
            DT_INTERFACE if len >= 9 => {
                let is_vc = d[5] == CLASS_VIDEO && d[6] == SUBCLASS_VIDEOCONTROL;
                if is_vc && info.is_none() {
                    info = Some(VcInfo {
                        interface: d[2],
                        ..Default::default()
                    });
                    in_vc = true;
                } else {
                    in_vc = false;
                }
            }
            DT_CS_INTERFACE if in_vc && len >= 3 => {
                let vc = info.as_mut()?;
                match d[2] {
                    // bTerminalID, wTerminalType, bAssocTerminal, iTerminal,
                    // then for cameras: wObjectiveFocalLengthMin/Max,
                    // wOcularFocalLength, bControlSize, bmControls.
                    VC_INPUT_TERMINAL if len >= 15 => {
                        let ttype = u16::from_le_bytes([d[4], d[5]]);
                        if ttype == ITT_CAMERA && vc.camera_terminal.is_none() {
                            vc.camera_terminal = Some(Entity {
                                id: d[3],
                                controls: bitmap(&d[15..], d[14] as usize),
                            });
                        }
                    }
                    // bUnitID, bSourceID, wMaxMultiplier, bControlSize, bmControls.
                    VC_PROCESSING_UNIT if len >= 8 && vc.processing_unit.is_none() => {
                        vc.processing_unit = Some(Entity {
                            id: d[3],
                            controls: bitmap(&d[8..], d[7] as usize),
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        pos += len;
    }
    info
}

fn bitmap(bytes: &[u8], size: usize) -> u32 {
    bytes
        .iter()
        .take(size.min(4))
        .enumerate()
        .fold(0u32, |acc, (i, b)| acc | ((*b as u32) << (8 * i)))
}

/// Which entity a control belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    CameraTerminal,
    ProcessingUnit,
}

/// The companion "automatic" control of a value control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoKind {
    /// A one-byte boolean control (1 = automatic).
    Flag,
    /// `CT_AE_MODE_CONTROL`: a one-byte bitmap; 1 = manual, 2 = auto,
    /// 4 = shutter priority, 8 = aperture priority.
    AeMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoControl {
    pub selector: u8,
    /// Bit in `bmControls`.
    pub bit: u8,
    pub kind: AutoKind,
}

/// One control reachable through a query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlDef {
    /// Query parameter name.
    pub name: &'static str,
    pub unit: Unit,
    pub selector: u8,
    /// Bit in `bmControls`.
    pub bit: u8,
    /// Size of the value in bytes (the whole control unless `offset` is set).
    pub len: u8,
    /// Byte offset of this value inside a larger control (pan and tilt share
    /// `CT_PANTILT_ABSOLUTE_CONTROL`).
    pub offset: u8,
    /// Total size of the control's data.
    pub total: u8,
    pub signed: bool,
    pub auto: Option<AutoControl>,
}

const fn simple(
    name: &'static str,
    unit: Unit,
    selector: u8,
    bit: u8,
    len: u8,
    signed: bool,
    auto: Option<AutoControl>,
) -> ControlDef {
    ControlDef {
        name,
        unit,
        selector,
        bit,
        len,
        offset: 0,
        total: len,
        signed,
        auto,
    }
}

use Unit::{CameraTerminal as CT, ProcessingUnit as PU};

/// Controls in the order `/controls` lists them. Selectors and bits are from
/// USB Video Class 1.5, tables 4-5 (camera terminal) and 4-11 (processing
/// unit) and the `bmControls` layouts in 3-6 and 3-8.
pub const CONTROLS: &[ControlDef] = &[
    simple(
        "exposure",
        CT,
        0x04,
        3,
        4,
        false,
        Some(AutoControl {
            selector: 0x02,
            bit: 1,
            kind: AutoKind::AeMode,
        }),
    ),
    simple(
        "focus",
        CT,
        0x06,
        5,
        2,
        false,
        Some(AutoControl {
            selector: 0x08,
            bit: 17,
            kind: AutoKind::Flag,
        }),
    ),
    simple("iris", CT, 0x09, 7, 2, false, None),
    simple("zoom", CT, 0x0b, 9, 2, false, None),
    ControlDef {
        name: "pan",
        unit: CT,
        selector: 0x0d,
        bit: 11,
        len: 4,
        offset: 0,
        total: 8,
        signed: true,
        auto: None,
    },
    ControlDef {
        name: "tilt",
        unit: CT,
        selector: 0x0d,
        bit: 11,
        len: 4,
        offset: 4,
        total: 8,
        signed: true,
        auto: None,
    },
    simple("roll", CT, 0x0f, 13, 2, true, None),
    simple("brightness", PU, 0x02, 0, 2, true, None),
    simple("contrast", PU, 0x03, 1, 2, false, None),
    simple(
        "hue",
        PU,
        0x06,
        2,
        2,
        true,
        Some(AutoControl {
            selector: 0x10,
            bit: 11,
            kind: AutoKind::Flag,
        }),
    ),
    simple("saturation", PU, 0x07, 3, 2, false, None),
    simple("sharpness", PU, 0x08, 4, 2, false, None),
    simple("gamma", PU, 0x09, 5, 2, false, None),
    simple(
        "wb",
        PU,
        0x0a,
        6,
        2,
        false,
        Some(AutoControl {
            selector: 0x0b,
            bit: 12,
            kind: AutoKind::Flag,
        }),
    ),
    simple("backlight", PU, 0x01, 8, 2, false, None),
    simple("gain", PU, 0x04, 9, 2, false, None),
    simple("powerline", PU, 0x05, 10, 1, false, None),
];

/// Query parameter names that are not controls but are accepted on `/`.
pub const OTHER_PARAMS: &[&str] = &["t", "_"];

/// A requested value for a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Auto,
    Value(i64),
}

pub fn find(name: &str) -> Option<&'static ControlDef> {
    CONTROLS.iter().find(|c| c.name == name)
}

/// Interprets one query parameter. `powerline` takes the mains frequency
/// (`0`, `50`, `60`) instead of the raw code.
pub fn parse_setting(name: &str, value: &str) -> Result<(&'static ControlDef, Setting), String> {
    let def = find(name).ok_or_else(|| {
        format!(
            "unknown parameter '{name}'; known: {}",
            CONTROLS
                .iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if value.eq_ignore_ascii_case("auto") {
        if def.auto.is_none() {
            return Err(format!("{name} has no automatic mode"));
        }
        return Ok((def, Setting::Auto));
    }
    let v: i64 = match (name, value) {
        ("powerline", "0" | "off") => 0,
        ("powerline", "50") => 1,
        ("powerline", "60") => 2,
        ("powerline", _) => return Err("powerline must be 0, 50 or 60".into()),
        _ => value
            .parse()
            .map_err(|_| format!("{name} must be a number or 'auto'"))?,
    };
    let bits = def.len as u32 * 8;
    let (lo, hi) = if def.signed {
        (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
    } else {
        (0, (1i64 << bits) - 1)
    };
    if v < lo || v > hi {
        return Err(format!("{name} must be between {lo} and {hi}"));
    }
    Ok((def, Setting::Value(v)))
}

/// Reads a control value out of the control's data.
pub fn decode(def: &ControlDef, data: &[u8]) -> Option<i64> {
    let (o, n) = (def.offset as usize, def.len as usize);
    let b = data.get(o..o + n)?;
    let mut raw = [0u8; 8];
    raw[..n].copy_from_slice(b);
    let u = u64::from_le_bytes(raw);
    Some(if def.signed && n < 8 && (u >> (n * 8 - 1)) & 1 == 1 {
        (u as i64) - (1i64 << (n * 8))
    } else {
        u as i64
    })
}

/// Writes a control value into the control's data.
pub fn encode(def: &ControlDef, value: i64, data: &mut [u8]) {
    let (o, n) = (def.offset as usize, def.len as usize);
    data[o..o + n].copy_from_slice(&value.to_le_bytes()[..n]);
}

/// Translates the raw power line code back for `/controls`.
pub fn display_value(def: &ControlDef, raw: i64) -> i64 {
    match (def.name, raw) {
        ("powerline", 1) => 50,
        ("powerline", 2) => 60,
        _ => raw,
    }
}

/// Picks the AE mode bit to set for "automatic exposure" from the bitmap of
/// modes the camera supports (from GET_RES): aperture priority is what
/// webcams implement; plain auto is the fallback.
pub fn auto_ae_mode(supported: u8) -> Option<u8> {
    [8u8, 2, 4].into_iter().find(|m| supported & m != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configuration descriptor with a VideoControl interface holding a
    /// camera terminal (ID 1) and a processing unit (ID 2).
    fn descriptor() -> Vec<u8> {
        let mut d = vec![
            // configuration
            9, 0x02, 0, 0, 2, 1, 0, 0x80, 250, // interface association
            8, 0x0b, 0, 2, 0x0e, 0x03, 0, 0, // VC interface 0
            9, 0x04, 0, 0, 1, 0x0e, 0x01, 0, 0, // VC header
            13, 0x24, 0x01, 0x10, 0x01, 0, 0, 0, 0, 0, 0, 1, 1,
            // camera terminal: ID 1, ITT_CAMERA, controls 3 bytes: 0x2a 0x02 0x02
            // (AE mode, exposure abs, focus abs, zoom abs, focus auto)
            18, 0x24, 0x02, 1, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0x2a, 0x02, 0x02,
            // processing unit: ID 2, source 1, controls 2 bytes 0x5f 0x10
            // (brightness, contrast, hue, saturation, sharpness, wb temp, wb temp auto)
            11, 0x24, 0x05, 2, 1, 0, 0, 2, 0x5f, 0x10, 0, // output terminal
            9, 0x24, 0x03, 3, 0x01, 0x01, 0, 2, 0, // VS interface 1
            9, 0x04, 1, 0, 0, 0x0e, 0x02, 0, 0,
        ];
        let len = d.len() as u16;
        d[2..4].copy_from_slice(&len.to_le_bytes());
        d
    }

    #[test]
    fn finds_units() {
        let vc = parse_vc(&descriptor()).unwrap();
        assert_eq!(vc.interface, 0);
        assert_eq!(
            vc.camera_terminal,
            Some(Entity {
                id: 1,
                controls: 0x0002_022a
            })
        );
        assert_eq!(
            vc.processing_unit,
            Some(Entity {
                id: 2,
                controls: 0x105f
            })
        );
        assert_eq!(parse_vc(&[9, 0x02, 9, 0, 0, 1, 0, 0x80, 250]), None);
        // Truncated descriptors do not panic.
        assert!(parse_vc(&descriptor()[..40]).is_some());
    }

    #[test]
    fn settings() {
        let (d, s) = parse_setting("wb", "auto").unwrap();
        assert_eq!(d.name, "wb");
        assert_eq!(s, Setting::Auto);
        let (d, s) = parse_setting("brightness", "-10").unwrap();
        assert_eq!(d.unit, Unit::ProcessingUnit);
        assert_eq!(s, Setting::Value(-10));
        assert_eq!(
            parse_setting("powerline", "60").unwrap().1,
            Setting::Value(2)
        );
        assert!(parse_setting("zoom", "auto").is_err());
        assert!(parse_setting("zoom", "70000").is_err());
        assert!(parse_setting("contrast", "-1").is_err());
        assert!(parse_setting("bogus", "1").is_err());
        assert!(parse_setting("focus", "abc").is_err());
    }

    #[test]
    fn codec() {
        let b = find("brightness").unwrap();
        let mut data = [0u8; 2];
        encode(b, -5, &mut data);
        assert_eq!(data, [0xfb, 0xff]);
        assert_eq!(decode(b, &data), Some(-5));
        let z = find("zoom").unwrap();
        encode(z, 300, &mut data);
        assert_eq!(decode(z, &data), Some(300));
        let mut pt = [0u8; 8];
        encode(find("pan").unwrap(), -36000, &mut pt);
        encode(find("tilt").unwrap(), 1800, &mut pt);
        assert_eq!(decode(find("pan").unwrap(), &pt), Some(-36000));
        assert_eq!(decode(find("tilt").unwrap(), &pt), Some(1800));
        assert_eq!(decode(z, &[1]), None);
        assert_eq!(display_value(find("powerline").unwrap(), 2), 60);
        assert_eq!(auto_ae_mode(0x0a), Some(8));
        assert_eq!(auto_ae_mode(0x03), Some(2));
        assert_eq!(auto_ae_mode(0x01), None);
    }
}
