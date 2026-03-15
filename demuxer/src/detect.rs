use crate::types::ContainerFormat;

/// Detect the container format from the first bytes of data (magic bytes).
pub fn detect_format(data: &[u8]) -> ContainerFormat {
    if data.len() < 8 {
        return ContainerFormat::Unknown;
    }

    // MP4: "ftyp" at bytes 4..8
    if &data[4..8] == b"ftyp" {
        return ContainerFormat::Mp4;
    }

    // MKV/WebM: EBML header 0x1A45DFA3
    if data.len() >= 4 && data[0] == 0x1A && data[1] == 0x45 && data[2] == 0xDF && data[3] == 0xA3
    {
        // Could distinguish MKV vs WebM via DocType, but for now treat as Mkv
        return ContainerFormat::Mkv;
    }

    ContainerFormat::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_mp4_ftyp() {
        // Real MP4 header: box size (4 bytes) + "ftyp" + brand
        let data = [
            0x00, 0x00, 0x00, 0x1C, // box size = 28
            b'f', b't', b'y', b'p', // "ftyp"
            b'i', b's', b'o', b'm', // brand "isom"
            0x00, 0x00, 0x02, 0x00, // minor version
        ];
        assert_eq!(detect_format(&data), ContainerFormat::Mp4);
    }

    #[test]
    fn detect_mp4_ftyp_mp42() {
        let data = [
            0x00, 0x00, 0x00, 0x20, // box size
            b'f', b't', b'y', b'p', // "ftyp"
            b'm', b'p', b'4', b'2', // brand "mp42"
            0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(detect_format(&data), ContainerFormat::Mp4);
    }

    #[test]
    fn detect_mkv_ebml() {
        // EBML header for MKV
        let data = [
            0x1A, 0x45, 0xDF, 0xA3, // EBML magic
            0x93, // EBML size (VINT)
            0x42, 0x86, 0x81, 0x01, // EBMLVersion = 1
            0x42, 0xF7, 0x81, 0x01, // EBMLReadVersion = 1
        ];
        assert_eq!(detect_format(&data), ContainerFormat::Mkv);
    }

    #[test]
    fn detect_unknown_random_bytes() {
        let data = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(detect_format(&data), ContainerFormat::Unknown);
    }

    #[test]
    fn detect_unknown_too_short() {
        let data = [0x1A, 0x45, 0xDF];
        assert_eq!(detect_format(&data), ContainerFormat::Unknown);
    }

    #[test]
    fn detect_unknown_empty() {
        assert_eq!(detect_format(&[]), ContainerFormat::Unknown);
    }
}
