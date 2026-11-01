//! Parser for Wi-Fi Display information elements, as defined by the Wi-Fi
//! Display specification. The bytes come from any device in radio range, so
//! they are untrusted: nothing here may panic or read out of bounds.

const DEVICE_INFO_SUBELEMENT: u8 = 0;
const DEVICE_INFO_LENGTH: usize = 6;
const SUBELEMENT_HEADER_LENGTH: usize = 3;
const DEVICE_TYPE_MASK: u16 = 0b11;
const AVAILABILITY_SHIFT: u16 = 4;
const AVAILABILITY_MASK: u16 = 0b11;
const AVAILABLE_FOR_SESSION: u16 = 0b01;

/// How this computer describes itself: a source with RTSP on port 7236. These
/// are the bytes GNOME Network Displays sends, known to work with real TVs.
pub const SOURCE_IES: [u8; 9] = [0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Source,
    PrimarySink,
    SecondarySink,
    DualRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_type: DeviceType,
    pub available: bool,
    pub rtsp_port: u16,
    pub max_throughput_mbps: u16,
}

impl DeviceInfo {
    /// Whether the device can show what this computer sends.
    pub fn is_sink(&self) -> bool {
        matches!(
            self.device_type,
            DeviceType::PrimarySink | DeviceType::DualRole
        )
    }
}

/// Finds the device information among the subelements. Returns `None` if it is
/// missing or malformed, or if the data is cut short before reaching it.
pub fn parse_device_info(mut ies: &[u8]) -> Option<DeviceInfo> {
    loop {
        let (header, rest) = ies.split_at_checked(SUBELEMENT_HEADER_LENGTH)?;
        let length = usize::from(u16::from_be_bytes([*header.get(1)?, *header.get(2)?]));
        let (body, rest) = rest.split_at_checked(length)?;
        if *header.first()? == DEVICE_INFO_SUBELEMENT {
            return parse_device_info_body(body);
        }
        ies = rest;
    }
}

fn parse_device_info_body(body: &[u8]) -> Option<DeviceInfo> {
    let &[
        info_high,
        info_low,
        port_high,
        port_low,
        rate_high,
        rate_low,
    ] = <&[u8; DEVICE_INFO_LENGTH]>::try_from(body).ok()?;
    let info = u16::from_be_bytes([info_high, info_low]);
    let device_type = match info & DEVICE_TYPE_MASK {
        0 => DeviceType::Source,
        1 => DeviceType::PrimarySink,
        2 => DeviceType::SecondarySink,
        _ => DeviceType::DualRole,
    };
    Some(DeviceInfo {
        device_type,
        available: (info >> AVAILABILITY_SHIFT) & AVAILABILITY_MASK == AVAILABLE_FOR_SESSION,
        rtsp_port: u16::from_be_bytes([port_high, port_low]),
        max_throughput_mbps: u16::from_be_bytes([rate_high, rate_low]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As reported by a Samsung AU7172 TV.
    const SAMSUNG_TV: [u8; 9] = [0, 0, 6, 1, 17, 28, 68, 0, 54];

    #[test]
    fn parses_a_real_tv() {
        let info = parse_device_info(&SAMSUNG_TV).unwrap();
        assert_eq!(
            info,
            DeviceInfo {
                device_type: DeviceType::PrimarySink,
                available: true,
                rtsp_port: 7236,
                max_throughput_mbps: 54,
            }
        );
        assert!(info.is_sink());
    }

    #[test]
    fn our_own_description_is_an_available_source() {
        let info = parse_device_info(&SOURCE_IES).unwrap();
        assert_eq!(info.device_type, DeviceType::Source);
        assert!(info.available);
        assert_eq!(info.rtsp_port, 7236);
        assert!(!info.is_sink());
    }

    #[test]
    fn finds_device_info_after_other_subelements() {
        let mut ies = vec![7, 0, 2, 0xAA, 0xBB];
        ies.extend_from_slice(&SAMSUNG_TV);
        assert!(parse_device_info(&ies).is_some());
    }

    #[test]
    fn every_device_type_is_recognized() {
        let types = [
            (0, DeviceType::Source, false),
            (1, DeviceType::PrimarySink, true),
            (2, DeviceType::SecondarySink, false),
            (3, DeviceType::DualRole, true),
        ];
        for (bits, device_type, is_sink) in types {
            let info = parse_device_info(&[0, 0, 6, 0, bits, 0, 0, 0, 0]).unwrap();
            assert_eq!(info.device_type, device_type);
            assert_eq!(info.is_sink(), is_sink);
        }
    }

    #[test]
    fn busy_device_is_not_available() {
        let info = parse_device_info(&[0, 0, 6, 0, 0x01, 0x1c, 0x44, 0, 0]).unwrap();
        assert!(!info.available);
    }

    #[test]
    fn every_truncation_is_rejected() {
        for length in 0..SAMSUNG_TV.len() {
            assert_eq!(parse_device_info(&SAMSUNG_TV[..length]), None, "{length}");
        }
    }

    #[test]
    fn wrong_device_info_length_is_rejected() {
        assert_eq!(parse_device_info(&[0, 0, 5, 1, 2, 3, 4, 5]), None);
        assert_eq!(parse_device_info(&[0, 0, 7, 1, 2, 3, 4, 5, 6, 7]), None);
        assert_eq!(parse_device_info(&[0, 0, 0]), None);
    }

    #[test]
    fn oversized_length_is_rejected() {
        assert_eq!(parse_device_info(&[0, 0xFF, 0xFF, 1, 2, 3]), None);
        assert_eq!(parse_device_info(&[7, 0xFF, 0xFF]), None);
    }

    #[test]
    fn missing_device_info_is_rejected() {
        assert_eq!(parse_device_info(&[]), None);
        assert_eq!(parse_device_info(&[7, 0, 1, 0xAA]), None);
    }

    /// A small deterministic substitute for fuzzing, until real fuzz targets
    /// exist: arbitrary bytes must never panic.
    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut state: u32 = 0x9E37_79B9;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for _ in 0..20_000 {
            let length = (next() % 64) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| next().to_le_bytes()[0]).collect();
            let _ = parse_device_info(&bytes);
        }
    }
}
