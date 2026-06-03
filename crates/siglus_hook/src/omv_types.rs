use std::{fs::File, io::Read, path::Path};

use bytemuck::AnyBitPattern;

use crate::debug_log;


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmvVideoFormat {
    Rgb,
    Rgba,
    Yuv,
    Unknown,
}
impl From<i32> for OmvVideoFormat {
    fn from(value: i32) -> Self {
        match value {
            0 => OmvVideoFormat::Rgb,
            1 => OmvVideoFormat::Rgba,
            2 => OmvVideoFormat::Yuv,
            _ => OmvVideoFormat::Unknown,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct OmvCSize {
    cx: i32,
    cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct OmvCPoint {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct OmvHeader {
    header_size: i32,
    major_version: u8,
    minor_version: u8,
    dummy: [u8; 2],
    keep00: [u8; 32],
    theora_type: i32,
    theora_size: OmvCSize,
    theora_center: OmvCPoint,
    theora_us_per_frame: i32,
    theora_serial_no: i32,
    theora_header_page_no: i32,
    theora_subheader_page_no: i32,
    theora_page_cnt: i32,
    theora_packet_cnt: i32,
    keep01: [u8; 32],
    vorbis_serial_no: i32,
    vorbis_header_page_no: i32,
    vorbis_subheader_page_no: i32,
    vorbis_page_cnt: i32,
    vorbis_packet_cnt: i32,
    keep02: [u8; 32],
}

impl OmvHeader {
    fn from_bytes(bytes: &[u8]) -> OmvHeader {
        assert!(bytes.len() >= OMV_HEADER_SIZE, "Header bytes are too short");
        bytemuck::pod_read_unaligned::<OmvHeader>(bytes).to_owned()
    }
}
#[derive(Clone, Copy)]
pub struct OmvVideoInfo {
    pub format: OmvVideoFormat,
    pub display_width: u32,
    pub display_height: u32,
    pub payload_offset: usize,
}
const THEROA_PAGE_ENTRY_SIZE: usize = 0x1c;
const THEROA_PACKET_ENTRY_SIZE: usize = 0x20;
impl OmvVideoInfo {
    pub fn from_path(path: &Path) -> Result<OmvVideoInfo, String> {
        debug_log(&format!(
            "Reading OMV header from file: {}",
            path.display()
        ));
        let mut file = File::open(path).map_err(|e| format!("Failed to open file: {e}"))?;
        let mut header_bytes = [0u8; OMV_HEADER_SIZE];
        file.read_exact(&mut header_bytes)
            .map_err(|e| format!("Failed to read header: {e}"))?;
        let header = OmvHeader::from_bytes(&header_bytes);
        if header.theora_size.cx <= 0 || header.theora_size.cy <= 0 {
            return Err("Invalid video dimensions in header".to_string());
        }
        if header.theora_page_cnt < 0 || header.theora_packet_cnt < 0 {
            return Err("Invalid page or packet count in header".to_string());
        }
        Ok(OmvVideoInfo {
            format: OmvVideoFormat::from(header.theora_type),
            display_width: header.theora_size.cx as u32,
            display_height: header.theora_size.cy as u32,
            payload_offset: OMV_HEADER_SIZE 
                + (header.theora_page_cnt as usize * THEROA_PAGE_ENTRY_SIZE) 
                + (header.theora_packet_cnt as usize * THEROA_PACKET_ENTRY_SIZE),
        })
    }
}
const _: () = assert!(OMV_HEADER_SIZE == 168);
const OMV_HEADER_SIZE: usize = size_of::<OmvHeader>();
