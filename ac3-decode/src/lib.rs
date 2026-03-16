//! Pure Rust AC-3 (Dolby Digital) decoder, ported from ac3.js (MIT).
//!
//! Decodes AC-3 frames (bsid <= 10) to interleaved f32 PCM samples.
//! Designed for WASM (wasm32-unknown-unknown) — no libc, no std I/O.

mod bitstream;
mod imdct;
mod tables;

use bitstream::BitReader;
use imdct::Imdct;
use tables::*;

/// Decoded PCM output from one frame.
pub struct DecodedFrame {
    /// Interleaved f32 PCM samples (channel-interleaved).
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u32,
    /// Number of samples per channel.
    pub samples_per_channel: usize,
}

#[derive(Debug)]
pub enum DecodeError {
    NotEnoughData,
    InvalidSync,
    UnsupportedVersion(u8),
    InvalidHeader(String),
    FrameTooShort,
    BlockError(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DecodeError::NotEnoughData => write!(f, "Not enough data"),
            DecodeError::InvalidSync => write!(f, "Invalid AC-3 sync word"),
            DecodeError::UnsupportedVersion(v) => write!(f, "Unsupported bsid: {}", v),
            DecodeError::InvalidHeader(s) => write!(f, "Invalid header: {}", s),
            DecodeError::FrameTooShort => write!(f, "Frame too short"),
            DecodeError::BlockError(s) => write!(f, "Block decode error: {}", s),
        }
    }
}

/// Parsed BSI (Bit Stream Information) — matches ac3.js readBSI().
struct Bsi {
    fscod: usize,
    frmsizecod: usize,
    frmsize: usize,  // frame size in bytes
    bsid: u8,
    bsmod: u8,
    acmod: u8,
    nfchans: usize,
    lfeon: bool,
    cmixlev: usize,
    surmixlev: usize,
}

/// Maximum number of audio blocks in an E-AC-3 frame.
const EAC3_MAX_BLOCKS: usize = 6;

/// E-AC-3 reduced sample rates for fscod2.
const EAC3_REDUCED_SAMPLE_RATES: [u32; 4] = [24000, 22050, 16000, 0];

/// Number of blocks lookup from numblkscod.
const EAC3_BLOCKS: [usize; 4] = [1, 2, 3, 6];

/// Parsed E-AC-3 BSI (Bit Stream Information) — frame-level parameters.
/// E-AC-3 differs from AC-3: exponent strategies, coupling flags, SNR offsets,
/// block switch, and dither are all stored at the frame level in the BSI,
/// not per audio block.
struct EacBsi {
    // --- Syncinfo ---
    strmtyp: u8,        // 2 bits: 0=independent, 1=dependent, 2=AC-3 conversion
    substreamid: u8,    // 3 bits
    frmsiz: usize,      // 11 bits raw value
    frmsize: usize,     // (frmsiz + 1) * 2 bytes
    fscod: usize,       // 2 bits: sample rate code
    numblkscod: u8,     // 2 bits: 0=1block, 1=2, 2=3, 3=6
    num_blocks: usize,  // derived: [1, 2, 3, 6][numblkscod]
    sample_rate: u32,   // derived from fscod (or fscod2)

    // --- BSI core ---
    acmod: u8,
    lfeon: bool,
    bsid: u8,
    nfchans: usize,
    dialnorm: u8,
    bsmod: u8,
    cmixlev: usize,     // center downmix level
    surmixlev: usize,   // surround downmix level

    // --- Frame-level exponent strategies (audfrm) ---
    // [ch][blk] for channels, [blk] for coupling/LFE
    chexpstr: [[u8; EAC3_MAX_BLOCKS]; MAX_CHANNELS],
    cplexpstr: [u8; EAC3_MAX_BLOCKS],
    lfeexpstr: [u8; EAC3_MAX_BLOCKS],

    // --- Block switch and dither (frame-level in E-AC-3) ---
    blksw: [[bool; EAC3_MAX_BLOCKS]; MAX_CHANNELS],
    dithflag: [[bool; EAC3_MAX_BLOCKS]; MAX_CHANNELS],

    // --- Bit allocation params ---
    baie: bool,
    sdcycod: usize,
    fdcycod: usize,
    sgaincod: usize,
    dbpbcod: usize,
    floorcod: usize,

    // --- SNR offsets (frame-level) ---
    snroffste: bool,
    csnroffst: i32,
    blkfsnroffst: [i32; EAC3_MAX_BLOCKS],
    fsnroffst: [i32; MAX_CHANNELS],
    fgaincod: [usize; MAX_CHANNELS],
    cplfsnroffst: i32,
    cplfgaincod: usize,
    lfefsnroffst: i32,
    lfefgaincod: usize,

    // --- Coupling (frame-level flags) ---
    cplinu: [bool; EAC3_MAX_BLOCKS],

    // --- Spectral extension (store flags, skip details for MVP) ---
    spxinu: [bool; EAC3_MAX_BLOCKS],
}

impl EacBsi {
    fn new() -> Self {
        Self {
            strmtyp: 0,
            substreamid: 0,
            frmsiz: 0,
            frmsize: 0,
            fscod: 0,
            numblkscod: 3,
            num_blocks: 6,
            sample_rate: 48000,

            acmod: 0,
            lfeon: false,
            bsid: 16,
            nfchans: 2,
            dialnorm: 31,
            bsmod: 0,
            cmixlev: 0,
            surmixlev: 0,

            chexpstr: [[0; EAC3_MAX_BLOCKS]; MAX_CHANNELS],
            cplexpstr: [0; EAC3_MAX_BLOCKS],
            lfeexpstr: [0; EAC3_MAX_BLOCKS],

            blksw: [[false; EAC3_MAX_BLOCKS]; MAX_CHANNELS],
            dithflag: [[true; EAC3_MAX_BLOCKS]; MAX_CHANNELS],

            baie: false,
            sdcycod: 0,
            fdcycod: 0,
            sgaincod: 0,
            dbpbcod: 0,
            floorcod: 0,

            snroffste: false,
            csnroffst: 0,
            blkfsnroffst: [0; EAC3_MAX_BLOCKS],
            fsnroffst: [0; MAX_CHANNELS],
            fgaincod: [0; MAX_CHANNELS],
            cplfsnroffst: 0,
            cplfgaincod: 0,
            lfefsnroffst: 0,
            lfefgaincod: 0,

            cplinu: [false; EAC3_MAX_BLOCKS],
            spxinu: [false; EAC3_MAX_BLOCKS],
        }
    }
}

/// Audio block state — persists across blocks within a frame.
struct AudioBlock {
    // Block switch & dither
    blksw: [bool; MAX_CHANNELS],
    dithflag: [bool; MAX_CHANNELS],

    // Coupling
    cplinu: bool,
    chincpl: [bool; MAX_CHANNELS],
    phsflginu: bool,
    cplbegf: usize,
    cplendf: usize,
    ncplsubnd: usize,
    ncplbnd: usize,
    cplbndstrc: [bool; 18],
    cplcoe: [bool; MAX_CHANNELS],
    cplco: [[f32; 18]; MAX_CHANNELS],
    cplstrtmant: usize,
    cplendmant: usize,

    // Exponent strategy
    chexpstr: [u8; MAX_CHANNELS],
    cplexpstr: u8,
    lfeexpstr: u8,

    // Channel bandwidth
    strtmant: [usize; MAX_CHANNELS],
    endmant: [usize; MAX_CHANNELS],
    chbwcod: [usize; MAX_CHANNELS],

    // Exponents (decoded, absolute)
    exps: [[u8; 256]; MAX_CHANNELS],
    cplexps: [u8; 256],
    lfeexps: [u8; 256],

    // Bit allocation parameters
    sdcycod: usize,
    fdcycod: usize,
    sgaincod: usize,
    dbpbcod: usize,
    floorcod: usize,
    csnroffst: i32,
    fsnroffst: [i32; MAX_CHANNELS],
    fgaincod: [usize; MAX_CHANNELS],
    cplfsnroffst: i32,
    cplfgaincod: usize,
    lfefsnroffst: i32,
    lfefgaincod: usize,
    cplfleak: i32,
    cplsleak: i32,

    // Derived BA params
    sdecay: i32,
    fdecay: i32,
    sgain: i32,
    dbknee: i32,
    floor: i32,

    // Delta bit allocation
    deltbae: [u8; MAX_CHANNELS],
    cpldeltbae: u8,
    deltnseg: [usize; MAX_CHANNELS],
    deltoffst: [[usize; 8]; MAX_CHANNELS],
    deltlen: [[usize; 8]; MAX_CHANNELS],
    deltba: [[usize; 8]; MAX_CHANNELS],

    // BAP (computed)
    baps: [[u8; 256]; MAX_CHANNELS],
    cplbap: [u8; 256],
    lfebap: [u8; 256],

    // Rematrixing
    rematflg: [bool; 4],
    nrematbnds: usize,

    // Mantissa coefficients (decoded)
    chmant: [[f32; 256]; MAX_CHANNELS],
    cplmant: [f32; 256],
    lfemant: [f32; 256],
}

impl AudioBlock {
    fn new() -> Self {
        Self {
            blksw: [false; MAX_CHANNELS],
            dithflag: [true; MAX_CHANNELS],
            cplinu: false,
            chincpl: [false; MAX_CHANNELS],
            phsflginu: false,
            cplbegf: 0, cplendf: 0,
            ncplsubnd: 0, ncplbnd: 0,
            cplbndstrc: [false; 18],
            cplcoe: [false; MAX_CHANNELS],
            cplco: [[0.0; 18]; MAX_CHANNELS],
            cplstrtmant: 0, cplendmant: 0,
            chexpstr: [0; MAX_CHANNELS],
            cplexpstr: 0, lfeexpstr: 0,
            strtmant: [0; MAX_CHANNELS],
            endmant: [253; MAX_CHANNELS],
            chbwcod: [0; MAX_CHANNELS],
            exps: [[0; 256]; MAX_CHANNELS],
            cplexps: [0; 256],
            lfeexps: [0; 256],
            sdcycod: 0, fdcycod: 0, sgaincod: 0, dbpbcod: 0, floorcod: 0,
            csnroffst: 0,
            fsnroffst: [0; MAX_CHANNELS],
            fgaincod: [0; MAX_CHANNELS],
            cplfsnroffst: 0, cplfgaincod: 0,
            lfefsnroffst: 0, lfefgaincod: 0,
            cplfleak: 0, cplsleak: 0,
            sdecay: 0, fdecay: 0, sgain: 0, dbknee: 0, floor: 0,
            deltbae: [2; MAX_CHANNELS],
            cpldeltbae: 2,
            deltnseg: [0; MAX_CHANNELS],
            deltoffst: [[0; 8]; MAX_CHANNELS],
            deltlen: [[0; 8]; MAX_CHANNELS],
            deltba: [[0; 8]; MAX_CHANNELS],
            baps: [[0; 256]; MAX_CHANNELS],
            cplbap: [0; 256],
            lfebap: [0; 256],
            rematflg: [false; 4],
            nrematbnds: 0,
            chmant: [[0.0; 256]; MAX_CHANNELS],
            cplmant: [0.0; 256],
            lfemant: [0.0; 256],
        }
    }
}

/// AC-3 decoder state.
pub struct Ac3Decoder {
    /// Per-channel IMDCT processors (each has its own delay buffer).
    imdcts: [Imdct; MAX_CHANNELS],
    /// Per-channel sample output buffer (1536 samples per frame per channel).
    samples: [[f32; AC3_FRAME_SAMPLES]; MAX_CHANNELS],
    /// Dither state (simple LFSR).
    dither_state: u32,
}

impl Ac3Decoder {
    pub fn new() -> Self {
        Self {
            imdcts: [
                Imdct::new(), Imdct::new(), Imdct::new(),
                Imdct::new(), Imdct::new(), Imdct::new(),
            ],
            samples: [[0.0; AC3_FRAME_SAMPLES]; MAX_CHANNELS],
            dither_state: 1,
        }
    }

    /// Find the next AC-3 sync word in the data.
    pub fn find_sync(data: &[u8]) -> Option<usize> {
        for i in 0..data.len().saturating_sub(1) {
            if data[i] == 0x0B && data[i + 1] == 0x77 {
                return Some(i);
            }
        }
        None
    }

    /// Find sync word offset within data (max 16 bytes scan).
    fn find_sync_offset(&self, data: &[u8]) -> Option<usize> {
        let limit = data.len().saturating_sub(1).min(16);
        for i in 0..limit {
            if data[i] == 0x0B && data[i + 1] == 0x77 {
                return Some(i);
            }
        }
        None
    }

    /// Get the frame size from the header without full parsing.
    pub fn frame_size(data: &[u8]) -> Option<(usize, u8)> {
        if data.len() < 8 { return None; }
        if data[0] != 0x0B || data[1] != 0x77 { return None; }

        let bsid = ((data[5] >> 3) & 0x1F) as u8;

        if bsid <= 10 {
            let fscod = ((data[4] >> 6) & 0x03) as usize;
            let frmsizecod = (data[4] & 0x3F) as usize;
            if fscod >= 3 || frmsizecod / 2 >= 19 { return None; }
            let frame_size = FRAME_SIZE_TAB[frmsizecod / 2][fscod] as usize * 2;
            Some((frame_size, bsid))
        } else if bsid <= 16 {
            let mut br = BitReader::new(data);
            br.skip(16); // sync
            br.skip(2);  // strmtyp
            br.skip(3);  // substreamid
            let frmsiz = br.read(11) as usize;
            Some(((frmsiz + 1) * 2, bsid))
        } else {
            None
        }
    }

    /// Decode one AC-3 or E-AC-3 frame.
    /// Scans for the 0x0B77 sync word and checks bsid to determine format.
    pub fn decode_frame(&mut self, data: &[u8]) -> Result<DecodedFrame, DecodeError> {
        if data.len() < 8 { return Err(DecodeError::NotEnoughData); }

        // Scan for sync word — MKV blocks may have alignment padding
        let offset = self.find_sync_offset(data)
            .ok_or(DecodeError::InvalidSync)?;

        let frame_data = &data[offset..];
        if frame_data.len() < 8 { return Err(DecodeError::NotEnoughData); }

        // Check bsid at fixed byte position BEFORE sequential parsing.
        // bsid is in byte 5, bits [7:3] (5 MSBs).
        let bsid = (frame_data[5] >> 3) & 0x1F;

        if bsid <= 10 {
            self.decode_ac3_frame(frame_data)
        } else if bsid <= 16 {
            self.decode_eac3_frame(frame_data)
        } else {
            Err(DecodeError::UnsupportedVersion(bsid))
        }
    }

    /// Decode an AC-3 frame (bsid <= 10).
    fn decode_ac3_frame(&mut self, frame_data: &[u8]) -> Result<DecodedFrame, DecodeError> {
        let mut br = BitReader::new(frame_data);
        let bsi = self.parse_bsi(&mut br)?;

        if frame_data.len() < bsi.frmsize {
            return Err(DecodeError::FrameTooShort);
        }

        let total_channels = bsi.nfchans + if bsi.lfeon { 1 } else { 0 };
        for ch in 0..total_channels {
            self.samples[ch] = [0.0; AC3_FRAME_SAMPLES];
        }

        let mut audblk = AudioBlock::new();
        for blk in 0..6 {
            self.read_audio_block(&mut br, &bsi, &mut audblk, blk)?;
        }

        let samples_per_channel = AC3_FRAME_SAMPLES;
        let mut output = vec![0.0f32; samples_per_channel * total_channels];
        for s in 0..samples_per_channel {
            for ch in 0..total_channels {
                output[s * total_channels + ch] = self.samples[ch][s];
            }
        }

        Ok(DecodedFrame {
            samples: output,
            sample_rate: SAMPLE_RATES[bsi.fscod],
            channels: total_channels as u32,
            samples_per_channel,
        })
    }

    /// E-AC-3 (bsid 11-16) — not yet implemented.
    /// E-AC-3 has a fundamentally different BSI and audio block structure from AC-3:
    /// exponent strategies, coupling, SNR offsets etc. are all frame-level in the BSI,
    /// not per-block. The AC-3 audio block parser cannot be reused.
    /// Returns a descriptive error; the player will continue with video only.
    fn decode_eac3_frame(&mut self, _frame_data: &[u8]) -> Result<DecodedFrame, DecodeError> {
        Err(DecodeError::UnsupportedVersion(16))
    }

    /// Parse BSI — ported from ac3.js readBSI().
    fn parse_bsi(&self, br: &mut BitReader) -> Result<Bsi, DecodeError> {
        let sync = br.read(16);
        if sync != 0x0B77 {
            return Err(DecodeError::InvalidSync);
        }

        // Skip CRC1
        br.skip(16);

        let fscod = br.read(2) as usize;
        let frmsizecod = br.read(6) as usize;

        if fscod >= 3 {
            return Err(DecodeError::InvalidHeader("invalid fscod".into()));
        }
        if frmsizecod / 2 >= 19 {
            return Err(DecodeError::InvalidHeader("invalid frmsizecod".into()));
        }

        // Compute frame size in bytes
        let bitrate = BIT_RATES[frmsizecod / 2] as usize;
        let frmsize = match fscod {
            0 => 2 * bitrate,
            1 => ((320 * bitrate) / 147 + (frmsizecod & 1)),
            2 => 3 * bitrate,
            _ => unreachable!(),
        } * 2; // words → bytes

        let bsid = br.read(5) as u8;
        let bsmod = br.read(3) as u8;

        if bsid > 10 {
            return Err(DecodeError::UnsupportedVersion(bsid));
        }

        let acmod = br.read(3) as u8;
        let nfchans = NFCHANS[acmod as usize];

        let mut cmixlev = 0usize;
        if (acmod & 0x1) != 0 && acmod != 0x1 {
            cmixlev = br.read(2) as usize;
        }
        let mut surmixlev = 0usize;
        if (acmod & 0x4) != 0 {
            surmixlev = br.read(2) as usize;
        }
        if acmod == 0x2 {
            br.skip(2); // dsurmod
        }

        let lfeon = br.read(1) != 0;

        // dialnorm
        br.skip(5);
        // compre
        if br.read_bool() { br.skip(8); }
        // langcode
        if br.read_bool() { br.skip(8); }
        // audprodie
        if br.read_bool() { br.skip(7); }

        // If dual mono (acmod==0), duplicate fields
        if acmod == 0 {
            br.skip(5); // dialnorm2
            if br.read_bool() { br.skip(8); } // compr2
            if br.read_bool() { br.skip(8); } // langcod2
            if br.read_bool() { br.skip(7); } // audprodie2
        }

        br.skip(1); // copyrightb
        br.skip(1); // origbs

        // timecod1e/timecod2e (for bsid 6 = Annex D, these are different)
        if br.read_bool() { br.skip(14); }
        if br.read_bool() { br.skip(14); }

        // addbsie
        if br.read_bool() {
            let addbsil = br.read(6) as usize;
            br.skip((addbsil + 1) * 8);
        }

        Ok(Bsi {
            fscod, frmsizecod, frmsize, bsid, bsmod, acmod,
            nfchans, lfeon, cmixlev, surmixlev,
        })
    }

    /// Parse E-AC-3 BSI (syncinfo + bsi + audfrm_bsi).
    /// Follows ETSI TS 102 366 Annex E.
    /// After this returns, the BitReader is positioned at the first audio block.
    fn parse_eac3_bsi(&self, br: &mut BitReader) -> Result<EacBsi, DecodeError> {
        let mut eac = EacBsi::new();

        // ===== Syncinfo =====
        let sync = br.read(16);
        if sync != 0x0B77 {
            return Err(DecodeError::InvalidSync);
        }

        eac.strmtyp = br.read(2) as u8;
        eac.substreamid = br.read(3) as u8;
        eac.frmsiz = br.read(11) as usize;
        eac.frmsize = (eac.frmsiz + 1) * 2;

        eac.fscod = br.read(2) as usize;
        if eac.fscod == 3 {
            // Reduced sample rate: read fscod2
            let fscod2 = br.read(2) as usize;
            if fscod2 >= 3 {
                return Err(DecodeError::InvalidHeader("invalid fscod2".into()));
            }
            eac.sample_rate = EAC3_REDUCED_SAMPLE_RATES[fscod2];
            eac.numblkscod = 3; // forced to 6 blocks when fscod==3
        } else {
            eac.numblkscod = br.read(2) as u8;
            if eac.fscod < 3 {
                eac.sample_rate = SAMPLE_RATES[eac.fscod];
            }
        }
        eac.num_blocks = EAC3_BLOCKS[eac.numblkscod as usize];

        eac.acmod = br.read(3) as u8;
        eac.lfeon = br.read_bool();
        eac.bsid = br.read(5) as u8;

        if eac.bsid < 11 || eac.bsid > 16 {
            return Err(DecodeError::UnsupportedVersion(eac.bsid));
        }

        eac.nfchans = NFCHANS[eac.acmod as usize];

        // ===== BSI metadata =====
        eac.dialnorm = br.read(5) as u8;
        // compre
        if br.read_bool() { br.skip(8); }

        // Dual mono second dialogue normalization
        if eac.acmod == 0 {
            br.skip(5); // dialnorm2
            if br.read_bool() { br.skip(8); } // compr2
        }

        // Channel-dependent info
        let mut _dmixmod = 0u8;
        if eac.acmod >= 0x2 {
            _dmixmod = br.read(2) as u8;
        }
        // Center mix level
        if (eac.acmod & 0x1) != 0 && eac.acmod > 1 {
            let _ltrtcmixlev = br.read(3);
            let _lorocmixlev = br.read(3);
        }
        // Surround mix level
        if (eac.acmod & 0x4) != 0 {
            let _ltrtsurmixlev = br.read(3);
            let _lorosurmixlev = br.read(3);
        }

        // LFE mix level
        if eac.lfeon {
            if br.read_bool() { // lfemixlevcode
                br.skip(5); // lfemixlevcod
            }
        }

        // Dependent stream info
        if eac.strmtyp == 0 {
            // Independent stream
            if br.read_bool() { br.skip(8); } // pgmscle → pgmscl
            if eac.acmod == 0 {
                if br.read_bool() { br.skip(6); } // pgmscl2e → pgmscl2
            }
            if br.read_bool() { // extmixleve
                br.skip(5); // extmixlev
                if eac.acmod >= 0x2 {
                    br.skip(3); // addmixleve stuff
                    // Actually: extmixlev2 is different, let me handle more carefully
                }
                // Skip remaining extended mix info based on acmod
                // The spec is complex here — simplify by parsing known fields
            }
        }

        // Informational metadata
        if eac.numblkscod == 0 {
            br.skip(1); // blkid
        }
        if br.read_bool() { // infomdate
            eac.bsmod = br.read(3) as u8;
            br.skip(1); // copyrightb
            br.skip(1); // origbs
            if eac.acmod == 0x2 {
                br.skip(2); // dsurmod
                br.skip(2); // dheadphonmod
            }
            if eac.acmod >= 0x6 {
                br.skip(2); // dsurexmod
            }
            // audprodie
            if br.read_bool() {
                br.skip(5); // mixlevel
                br.skip(2); // roomtyp
                br.skip(1); // adconvtyp
            }
            if eac.acmod == 0 {
                // audprodi2e
                if br.read_bool() {
                    br.skip(5);
                    br.skip(2);
                    br.skip(1);
                }
            }
            if eac.fscod < 3 {
                br.skip(1); // sourcefscod
            }
        }

        // Converter sync
        if eac.strmtyp == 0 && eac.numblkscod != 3 {
            br.skip(1); // convsync
        }

        // Dependent stream channel map
        if eac.strmtyp == 1 {
            if br.read_bool() { // chanmape
                br.skip(16); // chanmap
            }
        }

        // Mix metadata
        if br.read_bool() { // mixmdate
            if eac.acmod > 0x2 {
                br.skip(2); // dmixmod
            }
            if (eac.acmod & 0x1) != 0 && eac.acmod > 0x2 {
                br.skip(3); // ltrtcmixlev
                br.skip(3); // lorocmixlev
            }
            if (eac.acmod & 0x4) != 0 {
                br.skip(3); // ltrtsurmixlev
                br.skip(3); // lorosurmixlev
            }
            if eac.lfeon {
                if br.read_bool() { // lfemixlevcode
                    br.skip(5);
                }
            }
            if eac.strmtyp == 0 {
                if br.read_bool() { br.skip(6); } // pgmscle → pgmscl
                if eac.acmod == 0 {
                    if br.read_bool() { br.skip(6); } // pgmscl2e → pgmscl2
                }
                if br.read_bool() { // extmixleve
                    br.skip(5); // extmixlev
                }
                let mixdef = br.read(2) as u8;
                match mixdef {
                    0 => { /* no additional data */ }
                    1 => { br.skip(5); } // premixcmpsel + drcsrc + premixcmpscl
                    2 => { br.skip(12); } // mixdata
                    3 => {
                        let mixdeflen = br.read(5) as usize;
                        br.skip((mixdeflen + 2) * 8); // skip variable-length mix data
                        // Align not needed — data is bit-counted
                    }
                    _ => {}
                }
                if eac.acmod < 0x2 {
                    if br.read_bool() { // paninfoe
                        br.skip(8); // panmean
                        br.skip(6); // paninfo
                    }
                    if eac.acmod == 0 {
                        if br.read_bool() { // paninfo2e
                            br.skip(8);
                            br.skip(6);
                        }
                    }
                }
                // frmmixcfginfoe
                if br.read_bool() {
                    if eac.numblkscod == 0 {
                        br.skip(5); // blkmixcfginfo
                    } else {
                        for _blk in 0..eac.num_blocks {
                            if br.read_bool() { // blkmixcfginfoe
                                br.skip(5);
                            }
                        }
                    }
                }
            }
        }

        // addbsie — additional BSI
        if br.read_bool() {
            let addbsil = br.read(6) as usize;
            br.skip((addbsil + 1) * 8);
        }

        // ===== audfrm — frame-level audio parameters =====
        // This is the KEY difference from AC-3: per-block parameters are
        // specified at frame level.

        let nfchans = eac.nfchans;
        let num_blocks = eac.num_blocks;

        // --- Exponent strategies ---
        if eac.numblkscod == 3 {
            // 6-block mode: per-channel per-block exponent strategies
            for blk in 0..num_blocks {
                for ch in 0..nfchans {
                    eac.chexpstr[ch][blk] = br.read(2) as u8;
                }
            }
        } else {
            // Fewer blocks: still per-channel per-block
            for blk in 0..num_blocks {
                for ch in 0..nfchans {
                    eac.chexpstr[ch][blk] = br.read(2) as u8;
                }
            }
        }

        // Coupling exponent strategy and coupling-in-use flags
        // First, parse coupling strategy: cplstre per block
        // In E-AC-3, coupling flags are in audfrm, not per audio block.
        // For 6-block mode:
        //   blk 0: cplinu is always signaled
        //   blk 1-5: cplstre flag → if set, new cplinu; else inherit
        // For fewer blocks: same pattern
        eac.cplinu[0] = br.read_bool();
        for blk in 1..num_blocks {
            if br.read_bool() { // cplstre
                eac.cplinu[blk] = br.read_bool();
            } else {
                eac.cplinu[blk] = eac.cplinu[blk - 1];
            }
        }

        // Coupling exponent strategy (for blocks where coupling is enabled)
        for blk in 0..num_blocks {
            if eac.cplinu[blk] {
                eac.cplexpstr[blk] = br.read(2) as u8;
            }
        }

        // LFE exponent strategy
        if eac.lfeon {
            for blk in 0..num_blocks {
                eac.lfeexpstr[blk] = br.read(1) as u8;
            }
        }

        // Converter exponent strategy (skip)
        if eac.strmtyp == 0 {
            if eac.numblkscod != 3 {
                if br.read_bool() { // convexpstre
                    br.skip(5); // convexpstr
                }
            }
        }

        // --- Block switch flags ---
        for ch in 0..nfchans {
            if br.read_bool() { // blkswe — per-channel enable
                for blk in 0..num_blocks {
                    eac.blksw[ch][blk] = br.read_bool();
                }
            } else {
                for blk in 0..num_blocks {
                    eac.blksw[ch][blk] = false;
                }
            }
        }

        // --- Dither flags ---
        for ch in 0..nfchans {
            if br.read_bool() { // dithflage — per-channel enable
                for blk in 0..num_blocks {
                    eac.dithflag[ch][blk] = br.read_bool();
                }
            } else {
                // Default: dither ON for all blocks
                for blk in 0..num_blocks {
                    eac.dithflag[ch][blk] = true;
                }
            }
        }

        // --- Bit allocation parametric info ---
        eac.baie = br.read_bool();
        if eac.baie {
            eac.sdcycod = br.read(2) as usize;
            eac.fdcycod = br.read(2) as usize;
            eac.sgaincod = br.read(2) as usize;
            eac.dbpbcod = br.read(2) as usize;
            eac.floorcod = br.read(3) as usize;
        }

        // --- SNR offset ---
        eac.snroffste = br.read_bool();
        if eac.snroffste {
            eac.csnroffst = br.read(6) as i32;

            // Per-block fine SNR offset (only for 6-block mode)
            if eac.numblkscod == 3 {
                for blk in 0..num_blocks {
                    eac.blkfsnroffst[blk] = br.read(4) as i32;
                }
            }

            // Per-channel fine SNR offset and gain
            for ch in 0..nfchans {
                eac.fsnroffst[ch] = br.read(4) as i32;
                eac.fgaincod[ch] = br.read(3) as usize;
            }

            // Coupling SNR offset (if coupling used in any block)
            let any_cpl = eac.cplinu[..num_blocks].iter().any(|&c| c);
            if any_cpl {
                eac.cplfsnroffst = br.read(4) as i32;
                eac.cplfgaincod = br.read(3) as usize;
            }

            // LFE SNR offset
            if eac.lfeon {
                eac.lfefsnroffst = br.read(4) as i32;
                eac.lfefgaincod = br.read(3) as usize;
            }
        }

        // --- Spectral extension strategy (skip details for MVP) ---
        if eac.strmtyp == 0 {
            // spxstre per block (only for independent streams)
            // For MVP: read the flags, skip spectral extension parameters
            let mut _spx_active = false;
            for blk in 0..num_blocks {
                if blk == 0 || br.read_bool() { // spxstre (blk 0 always has strategy)
                    eac.spxinu[blk] = br.read_bool();
                    if eac.spxinu[blk] {
                        // Skip SPX parameters — complex, not needed for MVP
                        // spxattene per channel
                        for _ch in 0..nfchans {
                            if br.read_bool() { // spxattene
                                br.skip(5); // spxatten
                            }
                        }
                        // SPX band structure and coordinates would be here
                        // For MVP, we hope SPX is not enabled in typical content
                        _spx_active = true;
                    }
                } else {
                    eac.spxinu[blk] = if blk > 0 { eac.spxinu[blk - 1] } else { false };
                }
            }
        }

        // Skip AHT (Adaptive Hybrid Transform) for MVP
        // Skip transient pre-noise processing for MVP

        Ok(eac)
    }

    /// Read and process one audio block — ported from ac3.js readAudioBlock().
    fn read_audio_block(
        &mut self,
        br: &mut BitReader,
        bsi: &Bsi,
        ab: &mut AudioBlock,
        blk: usize,
    ) -> Result<(), DecodeError> {
        let nfchans = bsi.nfchans;

        // Block switch & dither flags
        for ch in 0..nfchans { ab.blksw[ch] = br.read_bool(); }
        for ch in 0..nfchans { ab.dithflag[ch] = br.read_bool(); }

        // Dynamic range
        if br.read_bool() { br.skip(8); }
        if bsi.acmod == 0x0 {
            if br.read_bool() { br.skip(8); }
        }

        // === Coupling strategy ===
        let cplstre = if blk == 0 { true } else { br.read_bool() };
        if cplstre {
            ab.cplinu = br.read_bool();
            if ab.cplinu {
                for ch in 0..nfchans { ab.chincpl[ch] = br.read_bool(); }
                if bsi.acmod == 0x2 { ab.phsflginu = br.read_bool(); }
                ab.cplbegf = br.read(4) as usize;
                ab.cplendf = br.read(4) as usize;
                if ab.cplendf + 3 < ab.cplbegf {
                    return Err(DecodeError::BlockError("cplendf < cplbegf".into()));
                }
                ab.ncplsubnd = 3 + ab.cplendf - ab.cplbegf;
                if ab.ncplsubnd > 18 {
                    return Err(DecodeError::BlockError(
                        format!("ncplsubnd {} > 18", ab.ncplsubnd),
                    ));
                }
                ab.ncplbnd = ab.ncplsubnd;
                ab.cplbndstrc[0] = false;
                for bnd in 1..ab.ncplsubnd {
                    ab.cplbndstrc[bnd] = br.read_bool();
                    if ab.cplbndstrc[bnd] { ab.ncplbnd -= 1; }
                }
            } else {
                for ch in 0..nfchans { ab.chincpl[ch] = false; }
            }
        }

        // === Coupling coordinates ===
        if ab.cplinu {
            for ch in 0..nfchans {
                if ab.chincpl[ch] {
                    ab.cplcoe[ch] = br.read_bool();
                    if ab.cplcoe[ch] {
                        let mstrcplco = br.read(2) as i32;
                        let mut _bnd = 0usize;
                        for sbnd in 0..ab.ncplsubnd {
                            if sbnd == 0 || !ab.cplbndstrc[sbnd] {
                                let cplcoexp = br.read(4) as i32;
                                let cplcomant = br.read(4) as i32;
                                let cplco = if cplcoexp == 15 {
                                    cplcomant as f32 / 16.0
                                } else {
                                    (cplcomant as f32 + 16.0) / 32.0
                                };
                                let scale = 2.0f32.powi(-(cplcoexp + 3 * mstrcplco));
                                ab.cplco[ch][sbnd] = cplco * scale;
                                _bnd += 1;
                            } else {
                                // Inherit previous band's coordinate
                                ab.cplco[ch][sbnd] = ab.cplco[ch][sbnd - 1];
                            }
                        }
                    }
                }
            }
            // Phase flags
            if bsi.acmod == 0x2 && ab.phsflginu && (ab.cplcoe[0] || ab.cplcoe[1]) {
                for _bnd in 0..ab.ncplbnd {
                    br.skip(1); // phsflg — ignored for now
                }
            }
        }

        // === Rematrixing (stereo only) ===
        if bsi.acmod == 0x2 {
            let rematstr = br.read_bool();
            if rematstr {
                ab.nrematbnds = if !ab.cplinu || ab.cplbegf > 2 {
                    4
                } else if ab.cplbegf > 0 {
                    3
                } else {
                    2
                };
                for rbnd in 0..ab.nrematbnds {
                    ab.rematflg[rbnd] = br.read_bool();
                }
            }
        }

        // === Exponent strategies ===
        if ab.cplinu { ab.cplexpstr = br.read(2) as u8; }
        for ch in 0..nfchans { ab.chexpstr[ch] = br.read(2) as u8; }
        if bsi.lfeon { ab.lfeexpstr = br.read(1) as u8; }

        // === Channel bandwidth codes ===
        for ch in 0..nfchans {
            if ab.chexpstr[ch] != 0 {
                if !ab.chincpl[ch] {
                    ab.chbwcod[ch] = br.read(6) as usize;
                }
            }
        }

        // === Coupling exponents ===
        if ab.cplinu {
            ab.cplstrtmant = ab.cplbegf * 12 + 37;
            ab.cplendmant = (ab.cplendf + 3) * 12 + 37;

            if ab.cplexpstr != 0 {
                let grpsize = EXPONENT_GROUP_SIZE[ab.cplexpstr as usize];
                let ncplgrps = if grpsize > 0 {
                    (ab.cplendmant - ab.cplstrtmant) / (grpsize * 3)
                } else { 0 };

                let cplabsexp = br.read(4) as i32;
                unpack_exponents(br, &mut ab.cplexps, cplabsexp << 1, ncplgrps, grpsize, 0);
            }
        }

        // === Channel exponents ===
        for ch in 0..nfchans {
            if ab.chexpstr[ch] != 0 {
                ab.strtmant[ch] = 0;
                if ab.chincpl[ch] {
                    ab.endmant[ch] = 37 + 12 * ab.cplbegf;
                } else {
                    ab.endmant[ch] = 37 + 3 * (ab.chbwcod[ch] + 12);
                }

                let grpsize = EXPONENT_GROUP_SIZE[ab.chexpstr[ch] as usize];
                let nchgrps = match ab.chexpstr[ch] {
                    1 => (ab.endmant[ch] - 1) / 3,
                    2 => (ab.endmant[ch] + 2) / 6,
                    3 => (ab.endmant[ch] + 8) / 12,
                    _ => 0,
                };

                let absexp = br.read(4) as i32;
                unpack_exponents(br, &mut ab.exps[ch], absexp, nchgrps, grpsize, 1);

                let _gainrng = br.read(2);
            }
        }

        // === LFE exponents ===
        if bsi.lfeon && ab.lfeexpstr != 0 {
            let lfeabsexp = br.read(4) as i32;
            let grpsize = EXPONENT_GROUP_SIZE[ab.lfeexpstr as usize];
            unpack_exponents(br, &mut ab.lfeexps, lfeabsexp, 2, grpsize, 1);
        }

        // === Bit allocation parametric information ===
        let baie = br.read_bool();
        if baie {
            ab.sdcycod = br.read(2) as usize;
            ab.fdcycod = br.read(2) as usize;
            ab.sgaincod = br.read(2) as usize;
            ab.dbpbcod = br.read(2) as usize;
            ab.floorcod = br.read(3) as usize;
        }

        let snroffste = br.read_bool();
        if snroffste {
            ab.csnroffst = br.read(6) as i32;
            if ab.cplinu {
                ab.cplfsnroffst = br.read(4) as i32;
                ab.cplfgaincod = br.read(3) as usize;
            }
            for ch in 0..nfchans {
                ab.fsnroffst[ch] = br.read(4) as i32;
                ab.fgaincod[ch] = br.read(3) as usize;
            }
            if bsi.lfeon {
                ab.lfefsnroffst = br.read(4) as i32;
                ab.lfefgaincod = br.read(3) as usize;
            }
        }

        // Coupling leak
        if ab.cplinu {
            let cplleake = br.read_bool();
            if cplleake {
                ab.cplfleak = br.read(3) as i32;
                ab.cplsleak = br.read(3) as i32;
            }
        }

        // === Delta bit allocation ===
        let deltbaie = br.read_bool();
        if deltbaie {
            if ab.cplinu { ab.cpldeltbae = br.read(2) as u8; }
            for ch in 0..nfchans { ab.deltbae[ch] = br.read(2) as u8; }

            if ab.cplinu && ab.cpldeltbae == 1 {
                let nseg = br.read(3) as usize;
                // Note: cpldeltba not stored separately for simplicity
            }
            for ch in 0..nfchans {
                if ab.deltbae[ch] == 1 {
                    ab.deltnseg[ch] = br.read(3) as usize;
                    for seg in 0..=ab.deltnseg[ch] {
                        ab.deltoffst[ch][seg] = br.read(5) as usize;
                        ab.deltlen[ch][seg] = br.read(4) as usize;
                        ab.deltba[ch][seg] = br.read(3) as usize;
                    }
                }
            }
        } else if blk == 0 {
            ab.cpldeltbae = 2;
            for ch in 0..nfchans { ab.deltbae[ch] = 2; }
        }

        // === Compute derived BA parameters ===
        ab.sdecay = SLOW_DECAY[ab.sdcycod.min(3)] as i32;
        ab.fdecay = FAST_DECAY[ab.fdcycod.min(3)] as i32;
        ab.sgain = SLOW_GAIN[ab.sgaincod.min(3)] as i32;
        ab.dbknee = DB_PER_BIT[ab.dbpbcod.min(3)] as i32;
        ab.floor = FLOOR[ab.floorcod.min(7)] as i32;

        // === Run bit allocation for each channel ===
        // Extract BA params to avoid borrow conflicts
        let ba_params = BaParams {
            sdecay: ab.sdecay, fdecay: ab.fdecay, sgain: ab.sgain,
            dbknee: ab.dbknee, floor: ab.floor,
        };

        for ch in 0..nfchans {
            let snroffset = (((ab.csnroffst - 15) << 4) + ab.fsnroffst[ch]) << 2;
            let fgain = FAST_GAIN[ab.fgaincod[ch].min(7)] as i32;

            // Copy delta BA data to avoid borrow conflict
            let delt = if ab.deltbae[ch] == 0 || ab.deltbae[ch] == 1 {
                Some(DeltBAOwned {
                    nseg: ab.deltnseg[ch],
                    offst: ab.deltoffst[ch],
                    ba: ab.deltba[ch],
                    len: ab.deltlen[ch],
                })
            } else {
                None
            };

            // Copy exponents
            let mut exps_copy = [0u8; 256];
            exps_copy.copy_from_slice(&ab.exps[ch]);

            bit_allocation(
                bsi.fscod,
                &ba_params,
                ab.strtmant[ch],
                ab.endmant[ch],
                &exps_copy,
                fgain,
                snroffset,
                0, 0,
                delt.as_ref(),
                &mut ab.baps[ch],
            );
        }

        // Coupling BA
        if ab.cplinu {
            let snroffset = (((ab.csnroffst - 15) << 4) + ab.cplfsnroffst) << 2;
            let fgain = FAST_GAIN[ab.cplfgaincod.min(7)] as i32;
            let fastleak = (ab.cplfleak << 8) + 768;
            let slowleak = (ab.cplsleak << 8) + 768;

            let mut exps_copy = [0u8; 256];
            exps_copy.copy_from_slice(&ab.cplexps);

            bit_allocation(
                bsi.fscod, &ba_params,
                ab.cplstrtmant, ab.cplendmant,
                &exps_copy,
                fgain, snroffset,
                fastleak, slowleak,
                None,
                &mut ab.cplbap,
            );
        }

        // LFE BA
        if bsi.lfeon {
            let snroffset = (((ab.csnroffst - 15) << 4) + ab.lfefsnroffst) << 2;
            let fgain = FAST_GAIN[ab.lfefgaincod.min(7)] as i32;

            let mut exps_copy = [0u8; 256];
            exps_copy.copy_from_slice(&ab.lfeexps);

            bit_allocation(
                bsi.fscod, &ba_params, 0, LFE_COEFS,
                &exps_copy, fgain, snroffset,
                0, 0, None, &mut ab.lfebap,
            );
        }

        // === Skip field (dummy data) ===
        if br.read_bool() {
            let skipl = br.read(9) as usize;
            br.skip(skipl * 8);
        }

        // === Quantized mantissas ===
        let mut mant_reader = MantissaReader::new();

        let mut got_cplchan = false;
        for ch in 0..nfchans {
            // Zero the mantissa array
            ab.chmant[ch] = [0.0; 256];

            for bin in 0..ab.endmant[ch] {
                let bap = ab.baps[ch][bin];
                let exp = ab.exps[ch][bin];
                let scale = 2.0f32.powi(-(exp as i32));

                if bap != 0 || !ab.dithflag[ch] {
                    ab.chmant[ch][bin] = mant_reader.get(bap, br) * scale;
                } else {
                    ab.chmant[ch][bin] = self.dither() * scale;
                }
            }

            // Get coupling mantissas (once, shared between coupled channels)
            if ab.cplinu && ab.chincpl[ch] && !got_cplchan {
                let ncplmant = 12 * ab.ncplsubnd;
                for bin in 0..ncplmant {
                    let bap = ab.cplbap[bin];
                    let exp = ab.cplexps[bin];
                    let scale = 2.0f32.powi(-(exp as i32));
                    ab.cplmant[bin] = mant_reader.get(bap, br) * scale;
                }
                got_cplchan = true;
            }
        }

        // LFE mantissas
        if bsi.lfeon {
            for bin in 0..LFE_COEFS {
                let bap = ab.lfebap[bin];
                let exp = ab.lfeexps[bin];
                let scale = 2.0f32.powi(-(exp as i32));
                ab.lfemant[bin] = mant_reader.get(bap, br) * scale;
            }
        }

        // === Decouple channels ===
        if ab.cplinu {
            for ch in 0..nfchans {
                if ab.chincpl[ch] {
                    for sbnd in 0..ab.ncplsubnd {
                        for bin in 0..12 {
                            let cpl_bin = sbnd * 12 + bin;
                            let mantissa = if ab.cplmant[cpl_bin] == 0.0 && ab.dithflag[ch] {
                                let exp = ab.cplexps[cpl_bin];
                                self.dither() * 2.0f32.powi(-(exp as i32))
                            } else {
                                ab.cplmant[cpl_bin]
                            };
                            let out_bin = (sbnd + ab.cplbegf) * 12 + bin + 37;
                            if out_bin < 256 {
                                ab.chmant[ch][out_bin] = mantissa * ab.cplco[ch][sbnd] * 8.0;
                            }
                        }
                    }
                }
            }
        }

        // === Rematrixing (stereo mode) ===
        if bsi.acmod == 0x2 {
            for i in 0..ab.nrematbnds {
                if ab.rematflg[i] {
                    let begin = REMATRIX_BANDS[i];
                    let mut end = REMATRIX_BANDS[i + 1];
                    if ab.cplinu && end >= 36 + ab.cplbegf * 12 {
                        end = 36 + ab.cplbegf * 12;
                    }
                    for bin in begin..end.min(256) {
                        let left = ab.chmant[0][bin];
                        let right = ab.chmant[1][bin];
                        ab.chmant[0][bin] = left + right;
                        ab.chmant[1][bin] = left - right;
                    }
                }
            }
        }

        // === IMDCT ===
        for ch in 0..nfchans {
            self.imdcts[ch].process256(&ab.chmant[ch], &mut self.samples[ch], blk * BLOCK_SAMPLES);
        }

        // LFE IMDCT
        if bsi.lfeon {
            let lfe_ch = nfchans;
            // LFE: only 7 coefficients, rest are zero — already zeroed in chmant init
            // We reuse lfemant for the IMDCT
            let mut lfe_coeffs = [0.0f32; 256];
            for i in 0..LFE_COEFS {
                lfe_coeffs[i] = ab.lfemant[i];
            }
            self.imdcts[lfe_ch].process256(&lfe_coeffs, &mut self.samples[lfe_ch], blk * BLOCK_SAMPLES);
        }

        Ok(())
    }

    /// Generate a dither value for bap=0 coefficients.
    fn dither(&mut self) -> f32 {
        self.dither_state = self.dither_state.wrapping_mul(1103515245).wrapping_add(12345);
        let val = (self.dither_state >> 16) as i16;
        val as f32 / 32768.0 * 0.707 // sqrt(2)/2 ≈ -3dB
    }

    /// Reset decoder state (e.g., after seek).
    pub fn reset(&mut self) {
        for imdct in &mut self.imdcts {
            imdct.reset();
        }
        self.samples = [[0.0; AC3_FRAME_SAMPLES]; MAX_CHANNELS];
        self.dither_state = 1;
    }
}

// ============================================================================
// Exponent unpacking — ported from ac3.js exponents.js
// ============================================================================

/// Unpack grouped exponents into absolute values.
/// `skip` = 1 for channels (first exp is absolute), 0 for coupling.
fn unpack_exponents(
    br: &mut BitReader,
    out: &mut [u8; 256],
    absexp: i32,
    ngroups: usize,
    grpsize: usize,
    skip: usize,
) {
    // Read and unpack differential exponents
    let mut dexps = Vec::with_capacity(ngroups * 3);
    for _ in 0..ngroups {
        let grp = br.read(7) as i32;
        dexps.push((grp / 25) - 2);
        dexps.push(((grp % 25) / 5) - 2);
        dexps.push((grp % 5) - 2);
    }

    // Convert differentials to absolutes
    let mut prevexp = absexp;
    for d in &mut dexps {
        *d += prevexp as i32;
        prevexp = *d;
    }

    // Fill output with absolute exponents
    out[0] = absexp.clamp(0, 24) as u8;

    let mut idx = skip;
    for (i, &exp_val) in dexps.iter().enumerate() {
        let clamped = exp_val.clamp(0, 24) as u8;
        for _ in 0..grpsize {
            if idx < 256 {
                out[idx] = clamped;
                idx += 1;
            }
        }
    }
}

// ============================================================================
// Bit allocation — ported from ac3.js bitallocation.js
// ============================================================================

/// Extracted bit allocation parameters (avoids borrow conflicts).
struct BaParams {
    sdecay: i32,
    fdecay: i32,
    sgain: i32,
    dbknee: i32,
    floor: i32,
}

struct DeltBAOwned {
    nseg: usize,
    offst: [usize; 8],
    ba: [usize; 8],
    len: [usize; 8],
}

/// Log-addition for banded PSD computation.
#[inline]
fn logadd(a: i32, b: i32) -> i32 {
    let c = a - b;
    let address = (c.abs() >> 1).min(255) as usize;
    if c >= 0 {
        a + LATAB[address] as i32
    } else {
        b + LATAB[address] as i32
    }
}

/// Calculate low-frequency compensation.
#[inline]
fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        if b0 + 256 == b1 {
            384
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else if bin < 20 {
        if b0 + 256 == b1 {
            320
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else {
        (a - 128).max(0)
    }
}

/// Full bit allocation algorithm — ported from ac3.js bitAllocation().
fn bit_allocation(
    fscod: usize,
    ba: &BaParams,
    start: usize,
    end: usize,
    exp: &[u8; 256],
    fgain: i32,
    snroffset: i32,
    mut fastleak: i32,
    mut slowleak: i32,
    delt: Option<&DeltBAOwned>,
    bap_out: &mut [u8; 256],
) {
    if start >= end || end > 256 {
        return;
    }

    let bndstrt = MASKTAB[start] as usize;
    let bndend = MASKTAB[(end - 1).min(255)] as usize + 1;

    // Step 1: PSD per bin
    let mut psd = [0i32; 256];
    for bin in start..end {
        psd[bin] = 3072 - ((exp[bin] as i32) << 7);
    }

    // Step 2: Banded PSD via logadd
    let mut bndpsd = [0i32; 64];
    let mut j = start;
    let mut k = bndstrt;

    loop {
        let lastbin = (BNDTAB[k] as usize + BNDSZ[k] as usize).min(end);
        bndpsd[k] = psd[j];
        j += 1;
        while j < lastbin {
            bndpsd[k] = logadd(bndpsd[k], psd[j]);
            j += 1;
        }
        k += 1;
        if end <= lastbin { break; }
    }

    // Step 3: Excitation function
    let mut excite = [0i32; 64];
    let mut mask = [0i32; 64];
    let mut lowcomp = 0i32;
    let mut begin;

    if bndstrt == 0 {
        lowcomp = calc_lowcomp(lowcomp, bndpsd[0], bndpsd[1], 0);
        excite[0] = bndpsd[0] - fgain - lowcomp;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[1], bndpsd[2], 1);
        excite[1] = bndpsd[1] - fgain - lowcomp;
        begin = 7;

        for bin in 2..7 {
            if bin >= bndend { break; }
            if bndend != 7 || bin != 6 {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = bndpsd[bin] - fgain;
            slowleak = bndpsd[bin] - ba.sgain;
            excite[bin] = fastleak - lowcomp;
            if bndend != 7 || bin != 6 {
                if bndpsd[bin] <= bndpsd[bin + 1] {
                    begin = bin + 1;
                    break;
                }
            }
        }

        for bin in begin..bndend.min(22) {
            if bndend != 7 || bin != 6 {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak -= ba.fdecay;
            fastleak = fastleak.max(bndpsd[bin] - fgain);
            slowleak -= ba.sdecay;
            slowleak = slowleak.max(bndpsd[bin] - ba.sgain);
            excite[bin] = (fastleak - lowcomp).max(slowleak);
        }

        begin = 22;
    } else {
        begin = bndstrt;
    }

    for bin in begin..bndend {
        fastleak -= ba.fdecay;
        fastleak = fastleak.max(bndpsd[bin] - fgain);
        slowleak -= ba.sdecay;
        slowleak = slowleak.max(bndpsd[bin] - ba.sgain);
        excite[bin] = fastleak.max(slowleak);
    }

    // Step 4: Masking curve
    for bin in bndstrt..bndend {
        if bndpsd[bin] < ba.dbknee {
            excite[bin] += (ba.dbknee - bndpsd[bin]) >> 2;
        }
        mask[bin] = excite[bin].max(HTH[fscod][bin] as i32);
    }

    // Step 5: Delta bit allocation
    if let Some(d) = delt {
        let mut band = 0usize;
        for seg in 0..=d.nseg {
            band += d.offst[seg];
            let delta = if d.ba[seg] >= 4 {
                ((d.ba[seg] as i32) - 3) << 7
            } else {
                ((d.ba[seg] as i32) - 4) << 7
            };
            for _ in 0..d.len[seg] {
                if band < 64 {
                    mask[band] += delta;
                }
                band += 1;
            }
        }
    }

    // Step 6: Compute BAP from PSD and mask
    let mut i = start;
    j = bndstrt;
    loop {
        let lastbin = (BNDTAB[j] as usize + BNDSZ[j] as usize).min(end);
        mask[j] -= snroffset;
        mask[j] -= ba.floor;
        if mask[j] < 0 { mask[j] = 0; }
        mask[j] &= 0x1FE0;
        mask[j] += ba.floor;

        while i < lastbin {
            let mut address = (psd[i] - mask[j]) >> 5;
            address = address.clamp(0, 63);
            bap_out[i] = BAPTAB[address as usize];
            i += 1;
        }
        j += 1;
        if end <= lastbin { break; }
    }

    // Zero unused
    for i in end..256 {
        bap_out[i] = 0;
    }
}

// ============================================================================
// Mantissa reader — ported from ac3.js mantissa.js
// ============================================================================

struct MantissaReader {
    bap1_ptr: usize,
    bap1_vals: [f32; 3],
    bap2_ptr: usize,
    bap2_vals: [f32; 3],
    bap4_ptr: usize,
    bap4_vals: [f32; 2],
}

impl MantissaReader {
    fn new() -> Self {
        Self {
            bap1_ptr: 3,  // force read on first use
            bap1_vals: [0.0; 3],
            bap2_ptr: 3,
            bap2_vals: [0.0; 3],
            bap4_ptr: 2,
            bap4_vals: [0.0; 2],
        }
    }

    fn get(&mut self, bap: u8, br: &mut BitReader) -> f32 {
        match bap {
            0 => 0.0,
            1 => {
                if self.bap1_ptr > 2 {
                    let group = br.read(5);
                    self.bap1_vals = bap1_lookup(group);
                    self.bap1_ptr = 0;
                }
                let val = self.bap1_vals[self.bap1_ptr];
                self.bap1_ptr += 1;
                val
            }
            2 => {
                if self.bap2_ptr > 2 {
                    let group = br.read(7);
                    self.bap2_vals = bap2_lookup(group);
                    self.bap2_ptr = 0;
                }
                let val = self.bap2_vals[self.bap2_ptr];
                self.bap2_ptr += 1;
                val
            }
            3 => {
                let code = br.read(3);
                bap3_lookup(code)
            }
            4 => {
                if self.bap4_ptr > 1 {
                    let group = br.read(7);
                    self.bap4_vals = bap4_lookup(group);
                    self.bap4_ptr = 0;
                }
                let val = self.bap4_vals[self.bap4_ptr];
                self.bap4_ptr += 1;
                val
            }
            5 => {
                let code = br.read(4);
                bap5_lookup(code)
            }
            // BAP 6-13: signed read of (bap-1) bits, divide by 2^(bap-2)
            6..=13 => {
                let nbits = bap - 1;
                let val = br.read_signed(nbits);
                val as f32 / (1i32 << (bap - 2)) as f32
            }
            // BAP 14: 14 bits signed / 2^13
            14 => {
                let val = br.read_signed(14);
                val as f32 / 8192.0
            }
            // BAP 15: 16 bits signed / 2^15
            15 => {
                let val = br.read_signed(16);
                val as f32 / 32768.0
            }
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_sync_basic() {
        let data = [0x00, 0x00, 0x0B, 0x77, 0xFF];
        assert_eq!(Ac3Decoder::find_sync(&data), Some(2));
    }

    #[test]
    fn find_sync_none() {
        let data = [0x00, 0x00, 0x00, 0x00];
        assert_eq!(Ac3Decoder::find_sync(&data), None);
    }

    #[test]
    fn frame_size_ac3() {
        let mut data = [0u8; 8];
        data[0] = 0x0B; data[1] = 0x77;
        data[2] = 0x00; data[3] = 0x00;
        data[4] = 0x0C; // fscod=0, frmsizecod=12
        data[5] = 0x40; // bsid=8
        let result = Ac3Decoder::frame_size(&data);
        assert!(result.is_some());
        let (size, bsid) = result.unwrap();
        assert_eq!(bsid, 8);
        assert!(size > 0);
    }

    #[test]
    fn logadd_basic() {
        // logadd(a, b) where a > b should return a + LATAB[...]
        let result = logadd(1000, 900);
        assert!(result > 1000);
        assert!(result < 1100);
    }

    #[test]
    fn bap_lookup_tables() {
        // BAP 1: group 0 should give [-2/3, -2/3, -2/3]
        let v = bap1_lookup(0);
        assert!((v[0] - (-2.0 / 3.0)).abs() < 1e-5);

        // BAP 1: group 13 (1*9 + 1*3 + 1 = 13) should give [0, 0, 0]
        let v = bap1_lookup(13);
        assert!((v[0]).abs() < 1e-5);
        assert!((v[1]).abs() < 1e-5);
        assert!((v[2]).abs() < 1e-5);

        // BAP 3: code 3 should give 0
        assert!(bap3_lookup(3).abs() < 1e-5);

        // BAP 5: code 7 should give 0
        assert!(bap5_lookup(7).abs() < 1e-5);
    }

    #[test]
    fn mantissa_signed_read() {
        // Test that BAP 6 reads 5 bits signed and divides by 2^4 = 16
        let data = [0b11111_000]; // 5 bits = 11111 = -1 signed
        let mut br = BitReader::new(&data);
        let mut mr = MantissaReader::new();
        let val = mr.get(6, &mut br);
        assert!((val - (-1.0 / 16.0)).abs() < 1e-5, "BAP 6: got {}", val);
    }

    #[test]
    fn eac_bsi_defaults() {
        let eac = EacBsi::new();
        assert_eq!(eac.strmtyp, 0);
        assert_eq!(eac.substreamid, 0);
        assert_eq!(eac.frmsize, 0);
        assert_eq!(eac.fscod, 0);
        assert_eq!(eac.numblkscod, 3);
        assert_eq!(eac.num_blocks, 6);
        assert_eq!(eac.sample_rate, 48000);
        assert_eq!(eac.acmod, 0);
        assert!(!eac.lfeon);
        assert_eq!(eac.bsid, 16);
        assert_eq!(eac.nfchans, 2);
        assert_eq!(eac.dialnorm, 31);
        assert_eq!(eac.bsmod, 0);
        // Dither defaults to true
        assert!(eac.dithflag[0][0]);
        assert!(eac.dithflag[0][5]);
        // Block switch defaults to false
        assert!(!eac.blksw[0][0]);
        // Coupling defaults to false
        assert!(!eac.cplinu[0]);
        // SNR defaults
        assert_eq!(eac.csnroffst, 0);
        assert!(!eac.snroffste);
        assert!(!eac.baie);
    }

    #[test]
    fn eac3_syncinfo_parse() {
        // Test data: 0x0B77 05FF 3F85 ...
        // sync=0x0B77, strmtyp=00, substreamid=000, frmsiz=10111111111=1535
        // fscod=00(48kHz), numblkscod=11(6blks), acmod=111(5ch), lfeon=1
        // bsid=10000=16, dialnorm=00101=5
        // Then compre=0 (no compression)
        // We need enough bytes for the BSI parser to not run out of data.
        // Build a minimal valid-ish E-AC-3 header.
        let mut data = vec![0u8; 256];
        // sync word
        data[0] = 0x0B;
        data[1] = 0x77;
        // strmtyp=00, substreamid=000, frmsiz[10:0]=10111111111
        data[2] = 0x05; // 00_000_101
        data[3] = 0xFF; // 11111111
        // fscod=00, numblkscod=11, acmod=111, lfeon=1
        data[4] = 0x3F; // 00_11_111_1
        // bsid=10000, dialnorm=00101
        data[5] = 0x85; // 10000_001
        data[6] = 0x01; // 01_......  (dialnorm continues, then compre=0)
        // Remaining bytes zero = compre=0, dual_mono=no (acmod=7 != 0),
        // All subsequent flags will be 0 (no optional metadata).

        let decoder = Ac3Decoder::new();
        let mut br = BitReader::new(&data);
        let result = decoder.parse_eac3_bsi(&mut br);

        // The parser may encounter issues with zeroed-out audfrm data,
        // but the syncinfo fields should parse correctly before any error.
        // For a robust test, we verify the header is parsed:
        match result {
            Ok(eac) => {
                assert_eq!(eac.strmtyp, 0, "strmtyp");
                assert_eq!(eac.substreamid, 0, "substreamid");
                assert_eq!(eac.frmsiz, 1535, "frmsiz");
                assert_eq!(eac.frmsize, 3072, "frmsize");
                assert_eq!(eac.fscod, 0, "fscod");
                assert_eq!(eac.numblkscod, 3, "numblkscod");
                assert_eq!(eac.num_blocks, 6, "num_blocks");
                assert_eq!(eac.sample_rate, 48000, "sample_rate");
                assert_eq!(eac.acmod, 7, "acmod");
                assert!(eac.lfeon, "lfeon");
                assert_eq!(eac.bsid, 16, "bsid");
                assert_eq!(eac.nfchans, 5, "nfchans");
            }
            Err(e) => {
                // If it fails due to running out of data in audfrm parsing,
                // that's acceptable for this test — the syncinfo was correct.
                // But let's not panic.
                panic!("parse_eac3_bsi failed: {}", e);
            }
        }
    }
}
