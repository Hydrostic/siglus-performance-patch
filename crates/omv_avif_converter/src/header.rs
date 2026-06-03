use std::fmt;
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::Path;
use bytemuck::AnyBitPattern;
#[repr(C)]
#[derive(Clone, Copy, Debug, AnyBitPattern)]
pub struct CSize {
    pub cx: i32,
    pub cy: i32,
}


#[repr(C)]
#[derive(Clone, Copy, Debug, AnyBitPattern)]
pub struct CPoint {
    pub x: i32,
    pub y: i32,
}


#[repr(C)]
#[derive(Clone, Copy, Debug, AnyBitPattern)]
pub struct OmvHeader {
    pub header_size: i32,
    pub major_version: u8,
    pub minor_version: u8,
    pub dummy: [u8; 2],
    pub keep00: [u8; 32],
    pub theora_type: i32,
    pub theora_size: CSize,
    pub theora_center: CPoint,
    pub theora_us_per_frame: i32,
    pub theora_serial_no: i32,
    pub theora_header_page_no: i32,
    pub theora_subheader_page_no: i32,
    pub theora_page_cnt: i32,
    pub theora_packet_cnt: i32,
    pub keep01: [u8; 32],
    pub vorbis_serial_no: i32,
    pub vorbis_header_page_no: i32,
    pub vorbis_subheader_page_no: i32,
    pub vorbis_page_cnt: i32,
    pub vorbis_packet_cnt: i32,
    pub keep02: [u8; 32],
}


pub const OMV_HEADER_SIZE: usize = size_of::<OmvHeader>();

const _: () = assert!(OMV_HEADER_SIZE == 168);

impl OmvHeader {
    pub fn read_from_file(path: impl AsRef<Path>) -> Result<Self, OmvReadError> {
        let bytes = fs::read(path)?;
        Self::read_from_prefix(&bytes)
    }

    pub fn read_from_prefix(bytes: &[u8]) -> Result<Self, OmvReadError> {
        if bytes.len() < OMV_HEADER_SIZE {
            return Err(OmvReadError::TooSmall {
                actual: bytes.len(),
                needed: OMV_HEADER_SIZE,
            });
        }

        Ok(bytemuck::pod_read_unaligned(&bytes[..OMV_HEADER_SIZE]))
    }

    pub fn theora_type_name(&self) -> &'static str {
        match self.theora_type {
            0 => "RGB",
            1 => "RGBA",
            2 => "YUV",
            _ => "unknown",
        }
    }
}

impl fmt::Display for OmvHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "header_size: {}", self.header_size)?;
        writeln!(f, "version: {}.{}", self.major_version, self.minor_version)?;
        writeln!(
            f,
            "theora_type: {} ({})",
            self.theora_type,
            self.theora_type_name()
        )?;
        writeln!(
            f,
            "theora_size: {}x{}",
            self.theora_size.cx, self.theora_size.cy
        )?;
        writeln!(
            f,
            "theora_center: {}, {}",
            self.theora_center.x, self.theora_center.y
        )?;
        writeln!(f, "theora_us_per_frame: {}", self.theora_us_per_frame)?;
        writeln!(f, "theora_serial_no: {}", self.theora_serial_no)?;
        writeln!(f, "theora_header_page_no: {}", self.theora_header_page_no)?;
        writeln!(
            f,
            "theora_subheader_page_no: {}",
            self.theora_subheader_page_no
        )?;
        writeln!(f, "theora_page_cnt: {}", self.theora_page_cnt)?;
        writeln!(f, "theora_packet_cnt: {}", self.theora_packet_cnt)?;
        writeln!(f, "vorbis_serial_no: {}", self.vorbis_serial_no)?;
        writeln!(f, "vorbis_header_page_no: {}", self.vorbis_header_page_no)?;
        writeln!(
            f,
            "vorbis_subheader_page_no: {}",
            self.vorbis_subheader_page_no
        )?;
        writeln!(f, "vorbis_page_cnt: {}", self.vorbis_page_cnt)?;
        write!(f, "vorbis_packet_cnt: {}", self.vorbis_packet_cnt)
    }
}

#[derive(Debug)]
pub enum OmvReadError {
    Io(io::Error),
    TooSmall { actual: usize, needed: usize },
}

impl fmt::Display for OmvReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::TooSmall { actual, needed } => {
                write!(f, "file is too small for OMV header: {actual} < {needed}")
            }
        }
    }
}

impl std::error::Error for OmvReadError {}

impl From<io::Error> for OmvReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
