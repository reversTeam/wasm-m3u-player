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
