use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const HOOK_DLL_NAME: &str = "siglus_hook.dll";
const HOOK_IMPORT_NAME: &str = "SiglusHookLoadAnchor";
const BACKUP_EXTENSION: &str = "siglus_iat_backup";
const IMAGE_DOS_SIGNATURE: &[u8; 2] = b"MZ";
const IMAGE_NT_SIGNATURE: &[u8; 4] = b"PE\0\0";
const IMAGE_NT_OPTIONAL_HDR32_MAGIC: u16 = 0x10B;
const IMAGE_NT_OPTIONAL_HDR64_MAGIC: u16 = 0x20B;
const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
const IMAGE_IMPORT_DESCRIPTOR_SIZE: usize = 20;
const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const HOOK_SECTION_NAME: &[u8; 8] = b".sighook";

#[derive(Clone, Debug)]
pub struct PatchStatus {
    pub exe_path: PathBuf,
    pub dll_path: PathBuf,
    pub dll_exists: bool,
    pub backup_path: PathBuf,
    pub backup_exists: bool,
    pub patched: bool,
    pub exe_machine: u16,
    pub dll_machine: Option<u16>,
}

impl PatchStatus {
    pub fn machine_summary(&self) -> String {
        match self.dll_machine {
            Some(dll_machine) => format!(
                "EXE: {}, DLL: {}",
                machine_name(self.exe_machine),
                machine_name(dll_machine)
            ),
            None => format!("EXE: {}, DLL: missing", machine_name(self.exe_machine)),
        }
    }
}

#[derive(Debug)]
pub enum PatchError {
    Io(io::Error),
    InvalidPe(String),
    AlreadyPatched,
    NotPatched,
    MissingHookDll(PathBuf),
    MissingBackup(PathBuf),
    ArchitectureMismatch { exe_machine: u16, dll_machine: u16 },
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::InvalidPe(message) => write!(formatter, "{message}"),
            Self::AlreadyPatched => {
                write!(formatter, "the executable already imports {HOOK_DLL_NAME}")
            }
            Self::NotPatched => write!(formatter, "the executable does not import {HOOK_DLL_NAME}"),
            Self::MissingHookDll(path) => {
                write!(
                    formatter,
                    "missing hook DLL next to the executable: {}",
                    path.display()
                )
            }
            Self::MissingBackup(path) => {
                write!(formatter, "missing backup for unpatch: {}", path.display())
            }
            Self::ArchitectureMismatch {
                exe_machine,
                dll_machine,
            } => write!(
                formatter,
                "architecture mismatch: EXE is {}, DLL is {}",
                machine_name(*exe_machine),
                machine_name(*dll_machine)
            ),
        }
    }
}

impl std::error::Error for PatchError {}

impl From<io::Error> for PatchError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn status(exe_path: &Path) -> Result<PatchStatus, PatchError> {
    let bytes = fs::read(exe_path)?;
    let pe = PeImage::parse(&bytes)?;
    let dll_path = hook_dll_path(exe_path)?;
    let dll_exists = dll_path.exists();
    let dll_machine = if dll_exists {
        Some(machine(&dll_path)?)
    } else {
        None
    };
    let backup_path = backup_path(exe_path);

    Ok(PatchStatus {
        exe_path: exe_path.to_path_buf(),
        dll_path,
        dll_exists,
        backup_exists: backup_path.exists(),
        backup_path,
        patched: pe.imports_dll(HOOK_DLL_NAME)? || pe.has_section(HOOK_SECTION_NAME),
        exe_machine: pe.machine,
        dll_machine,
    })
}

pub fn patch(exe_path: &Path) -> Result<(), PatchError> {
    let current = status(exe_path)?;
    if current.patched {
        return Err(PatchError::AlreadyPatched);
    }
    if !current.dll_exists {
        return Err(PatchError::MissingHookDll(current.dll_path));
    }
    let dll_machine = current.dll_machine.expect("dll exists");
    if current.exe_machine != dll_machine {
        return Err(PatchError::ArchitectureMismatch {
            exe_machine: current.exe_machine,
            dll_machine,
        });
    }

    if !current.backup_exists {
        fs::copy(exe_path, &current.backup_path)?;
    }

    let mut bytes = fs::read(exe_path)?;
    add_hook_import(&mut bytes)?;
    fs::write(exe_path, bytes)?;
    Ok(())
}

pub fn unpatch(exe_path: &Path) -> Result<(), PatchError> {
    let current = status(exe_path)?;
    if !current.patched {
        return Err(PatchError::NotPatched);
    }
    if !current.backup_exists {
        return Err(PatchError::MissingBackup(current.backup_path));
    }

    fs::copy(&current.backup_path, exe_path)?;
    Ok(())
}

pub fn machine(path: &Path) -> Result<u16, PatchError> {
    let bytes = fs::read(path)?;
    Ok(PeImage::parse(&bytes)?.machine)
}

pub fn machine_name(machine: u16) -> &'static str {
    match machine {
        0x014C => "x86",
        0x8664 => "x64",
        _ => "unknown",
    }
}

fn hook_dll_path(exe_path: &Path) -> Result<PathBuf, PatchError> {
    let parent = exe_path.parent().ok_or_else(|| {
        PatchError::InvalidPe(format!(
            "executable path has no parent directory: {}",
            exe_path.display()
        ))
    })?;
    Ok(parent.join(HOOK_DLL_NAME))
}

fn backup_path(exe_path: &Path) -> PathBuf {
    let mut backup = exe_path.to_path_buf();
    backup.set_extension(BACKUP_EXTENSION);
    backup
}

fn add_hook_import(bytes: &mut Vec<u8>) -> Result<(), PatchError> {
    let pe = PeImage::parse(bytes)?;
    if pe.imports_dll(HOOK_DLL_NAME)? || pe.has_section(HOOK_SECTION_NAME) {
        return Err(PatchError::AlreadyPatched);
    }

    let import_descriptors = pe.import_descriptors()?;
    let import_descriptor_bytes = if import_descriptors.count == 0 {
        Vec::new()
    } else {
        bytes[import_descriptors.offset..import_descriptors.offset + import_descriptors.count * 20]
            .to_vec()
    };

    let first_section_raw = pe
        .sections
        .iter()
        .map(|section| section.pointer_to_raw_data)
        .filter(|raw| *raw != 0)
        .min()
        .unwrap_or(pe.size_of_headers);
    let new_section_header_offset = pe.section_table_offset + pe.number_of_sections as usize * 40;
    if new_section_header_offset + 40 > first_section_raw as usize {
        return Err(PatchError::InvalidPe(
            "there is no room in the PE headers for a new section".to_string(),
        ));
    }

    let pointer_size = if pe.is_pe64 { 8usize } else { 4usize };
    let descriptor_count = import_descriptors.count + 2;
    let descriptors_size = descriptor_count * IMAGE_IMPORT_DESCRIPTOR_SIZE;
    let mut cursor = align_usize(descriptors_size, pointer_size);
    let ilt_offset = cursor;
    cursor += pointer_size * 2;
    let iat_offset = cursor;
    cursor += pointer_size * 2;
    cursor = align_usize(cursor, 2);
    let import_by_name_offset = cursor;
    cursor += 2 + HOOK_IMPORT_NAME.len() + 1;
    let dll_name_offset = cursor;
    cursor += HOOK_DLL_NAME.len() + 1;

    let virtual_size = cursor as u32;
    let raw_size = align_u32(virtual_size, pe.file_alignment);
    let raw_pointer = align_usize(bytes.len(), pe.file_alignment as usize);
    let virtual_address = pe.next_section_virtual_address();
    let new_section_rva = virtual_address;
    let file_header_offset = pe.file_header_offset;
    let number_of_sections = pe.number_of_sections;
    let section_table_offset = pe.section_table_offset;
    let size_of_image_offset = pe.size_of_image_offset;
    let checksum_offset = pe.checksum_offset;
    let import_directory_offset = pe.import_directory_offset;
    let section_alignment = pe.section_alignment;
    let _ = pe;

    if bytes.len() < raw_pointer {
        bytes.resize(raw_pointer, 0);
    }
    let section_start = bytes.len();
    bytes.resize(section_start + raw_size as usize, 0);
    let section = &mut bytes[section_start..section_start + raw_size as usize];

    section[..import_descriptor_bytes.len()].copy_from_slice(&import_descriptor_bytes);

    let new_descriptor_offset = import_descriptors.count * IMAGE_IMPORT_DESCRIPTOR_SIZE;
    let ilt_rva = new_section_rva + ilt_offset as u32;
    let iat_rva = new_section_rva + iat_offset as u32;
    let import_by_name_rva = new_section_rva + import_by_name_offset as u32;
    let dll_name_rva = new_section_rva + dll_name_offset as u32;

    write_u32(section, new_descriptor_offset, ilt_rva);
    write_u32(section, new_descriptor_offset + 12, dll_name_rva);
    write_u32(section, new_descriptor_offset + 16, iat_rva);

    write_thunk(section, ilt_offset, pointer_size, import_by_name_rva);
    write_thunk(section, iat_offset, pointer_size, import_by_name_rva);
    write_bytes(
        section,
        import_by_name_offset + 2,
        HOOK_IMPORT_NAME.as_bytes(),
    );
    section[import_by_name_offset + 2 + HOOK_IMPORT_NAME.len()] = 0;
    write_bytes(section, dll_name_offset, HOOK_DLL_NAME.as_bytes());
    section[dll_name_offset + HOOK_DLL_NAME.len()] = 0;

    write_section_header(
        bytes,
        section_table_offset + number_of_sections as usize * 40,
        virtual_size,
        virtual_address,
        raw_size,
        raw_pointer as u32,
    );
    write_u16(
        bytes,
        file_header_offset + 2,
        number_of_sections
            .checked_add(1)
            .ok_or_else(|| PatchError::InvalidPe("too many PE sections".to_string()))?,
    );
    write_u32(
        bytes,
        size_of_image_offset,
        align_u32(virtual_address + virtual_size, section_alignment),
    );
    write_u32(bytes, checksum_offset, 0);
    write_u32(bytes, import_directory_offset, new_section_rva);
    write_u32(bytes, import_directory_offset + 4, descriptors_size as u32);

    Ok(())
}

fn write_section_header(
    bytes: &mut [u8],
    offset: usize,
    virtual_size: u32,
    virtual_address: u32,
    size_of_raw_data: u32,
    pointer_to_raw_data: u32,
) {
    let header = &mut bytes[offset..offset + 40];
    header.fill(0);
    header[..8].copy_from_slice(HOOK_SECTION_NAME);
    write_u32(header, 8, virtual_size);
    write_u32(header, 12, virtual_address);
    write_u32(header, 16, size_of_raw_data);
    write_u32(header, 20, pointer_to_raw_data);
    write_u32(
        header,
        36,
        IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE,
    );
}

fn write_thunk(section: &mut [u8], offset: usize, pointer_size: usize, rva: u32) {
    if pointer_size == 8 {
        write_u64(section, offset, rva as u64);
    } else {
        write_u32(section, offset, rva);
    }
}

#[derive(Clone, Copy)]
struct ImportDescriptors {
    offset: usize,
    count: usize,
}

struct PeImage<'a> {
    bytes: &'a [u8],
    machine: u16,
    number_of_sections: u16,
    file_header_offset: usize,
    section_table_offset: usize,
    size_of_image_offset: usize,
    checksum_offset: usize,
    import_directory_offset: usize,
    import_directory_rva: u32,
    import_directory_size: u32,
    section_alignment: u32,
    file_alignment: u32,
    size_of_headers: u32,
    is_pe64: bool,
    sections: Vec<SectionHeader>,
}

impl<'a> PeImage<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, PatchError> {
        require_range(bytes, 0, 0x40)?;
        if &bytes[0..2] != IMAGE_DOS_SIGNATURE {
            return Err(PatchError::InvalidPe(
                "missing MZ DOS signature".to_string(),
            ));
        }

        let pe_offset = read_u32(bytes, 0x3C)? as usize;
        require_range(bytes, pe_offset, 24)?;
        if &bytes[pe_offset..pe_offset + 4] != IMAGE_NT_SIGNATURE {
            return Err(PatchError::InvalidPe("missing PE signature".to_string()));
        }

        let file_header_offset = pe_offset + 4;
        let machine = read_u16(bytes, file_header_offset)?;
        let number_of_sections = read_u16(bytes, file_header_offset + 2)?;
        let size_of_optional_header = read_u16(bytes, file_header_offset + 16)? as usize;
        let optional_header_offset = file_header_offset + 20;
        require_range(bytes, optional_header_offset, size_of_optional_header)?;

        let optional_magic = read_u16(bytes, optional_header_offset)?;
        let (is_pe64, data_directories_offset) = match optional_magic {
            IMAGE_NT_OPTIONAL_HDR32_MAGIC => (false, optional_header_offset + 96),
            IMAGE_NT_OPTIONAL_HDR64_MAGIC => (true, optional_header_offset + 112),
            _ => {
                return Err(PatchError::InvalidPe(format!(
                    "unsupported PE optional header magic: 0x{optional_magic:04X}"
                )));
            }
        };
        let import_directory_offset = data_directories_offset + IMAGE_DIRECTORY_ENTRY_IMPORT * 8;
        require_range(bytes, import_directory_offset, 8)?;

        let section_alignment = read_u32(bytes, optional_header_offset + 32)?;
        let file_alignment = read_u32(bytes, optional_header_offset + 36)?;
        let size_of_image_offset = optional_header_offset + 56;
        let checksum_offset = optional_header_offset + 64;
        let size_of_headers = read_u32(bytes, optional_header_offset + 60)?;
        let import_directory_rva = read_u32(bytes, import_directory_offset)?;
        let import_directory_size = read_u32(bytes, import_directory_offset + 4)?;

        if section_alignment == 0 || file_alignment == 0 {
            return Err(PatchError::InvalidPe(
                "PE section/file alignment cannot be zero".to_string(),
            ));
        }

        let section_table_offset = optional_header_offset + size_of_optional_header;
        let mut sections = Vec::with_capacity(number_of_sections as usize);
        for index in 0..number_of_sections as usize {
            let offset = section_table_offset + index * 40;
            require_range(bytes, offset, 40)?;
            let mut name = [0u8; 8];
            name.copy_from_slice(&bytes[offset..offset + 8]);
            sections.push(SectionHeader {
                name,
                virtual_size: read_u32(bytes, offset + 8)?,
                virtual_address: read_u32(bytes, offset + 12)?,
                size_of_raw_data: read_u32(bytes, offset + 16)?,
                pointer_to_raw_data: read_u32(bytes, offset + 20)?,
            });
        }

        Ok(Self {
            bytes,
            machine,
            number_of_sections,
            file_header_offset,
            section_table_offset,
            size_of_image_offset,
            checksum_offset,
            import_directory_offset,
            import_directory_rva,
            import_directory_size,
            section_alignment,
            file_alignment,
            size_of_headers,
            is_pe64,
            sections,
        })
    }

    fn imports_dll(&self, dll_name: &str) -> Result<bool, PatchError> {
        let dll_name = dll_name.to_ascii_lowercase();
        let descriptors = self.import_descriptors()?;
        for index in 0..descriptors.count {
            let descriptor_offset = descriptors.offset + index * IMAGE_IMPORT_DESCRIPTOR_SIZE;
            let name_rva = read_u32(self.bytes, descriptor_offset + 12)?;
            let name_offset = self.rva_to_offset(name_rva)?;
            let imported_name = read_c_string(self.bytes, name_offset)?.to_ascii_lowercase();
            if imported_name == dll_name {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn import_descriptors(&self) -> Result<ImportDescriptors, PatchError> {
        if self.import_directory_rva == 0 {
            return Ok(ImportDescriptors {
                offset: 0,
                count: 0,
            });
        }

        let offset = self.rva_to_offset(self.import_directory_rva)?;
        let mut count = 0usize;
        loop {
            let descriptor_offset = offset + count * IMAGE_IMPORT_DESCRIPTOR_SIZE;
            require_range(self.bytes, descriptor_offset, IMAGE_IMPORT_DESCRIPTOR_SIZE)?;
            if self.bytes[descriptor_offset..descriptor_offset + IMAGE_IMPORT_DESCRIPTOR_SIZE]
                .iter()
                .all(|byte| *byte == 0)
            {
                return Ok(ImportDescriptors { offset, count });
            }
            count += 1;
            if self.import_directory_size != 0
                && count * IMAGE_IMPORT_DESCRIPTOR_SIZE > self.import_directory_size as usize
            {
                return Err(PatchError::InvalidPe(
                    "import descriptor table is not null terminated".to_string(),
                ));
            }
        }
    }

    fn next_section_virtual_address(&self) -> u32 {
        let end = self
            .sections
            .iter()
            .map(|section| {
                section.virtual_address + section.virtual_size.max(section.size_of_raw_data)
            })
            .max()
            .unwrap_or(0);
        align_u32(end, self.section_alignment)
    }

    fn has_section(&self, name: &[u8; 8]) -> bool {
        self.sections.iter().any(|section| &section.name == name)
    }

    fn rva_to_offset(&self, rva: u32) -> Result<usize, PatchError> {
        for section in &self.sections {
            let span = section.virtual_size.max(section.size_of_raw_data);
            if rva >= section.virtual_address && rva < section.virtual_address + span {
                let offset = (rva - section.virtual_address) + section.pointer_to_raw_data;
                return Ok(offset as usize);
            }
        }

        if rva < self.size_of_headers {
            return Ok(rva as usize);
        }

        Err(PatchError::InvalidPe(format!(
            "RVA 0x{rva:08X} is outside all sections"
        )))
    }
}

#[derive(Clone, Copy)]
struct SectionHeader {
    name: [u8; 8],
    virtual_size: u32,
    virtual_address: u32,
    size_of_raw_data: u32,
    pointer_to_raw_data: u32,
}

fn require_range(bytes: &[u8], offset: usize, len: usize) -> Result<(), PatchError> {
    if offset
        .checked_add(len)
        .is_some_and(|end| end <= bytes.len())
    {
        Ok(())
    } else {
        Err(PatchError::InvalidPe(format!(
            "PE structure points outside the file at 0x{offset:X}"
        )))
    }
}

fn read_c_string(bytes: &[u8], offset: usize) -> Result<String, PatchError> {
    require_range(bytes, offset, 1)?;
    let end = bytes[offset..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|index| offset + index)
        .ok_or_else(|| PatchError::InvalidPe("unterminated import name".to_string()))?;
    Ok(String::from_utf8_lossy(&bytes[offset..end]).into_owned())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PatchError> {
    require_range(bytes, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PatchError> {
    require_range(bytes, offset, 4)?;
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_bytes(bytes: &mut [u8], offset: usize, value: &[u8]) {
    bytes[offset..offset + value.len()].copy_from_slice(value);
}

fn align_u32(value: u32, alignment: u32) -> u32 {
    if value == 0 {
        0
    } else {
        ((value + alignment - 1) / alignment) * alignment
    }
}

fn align_usize(value: usize, alignment: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value + alignment - 1) / alignment) * alignment
    }
}
