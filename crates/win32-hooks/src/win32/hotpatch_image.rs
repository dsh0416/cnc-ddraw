//! Read the hotpatch capability of a loaded PE32 image using its debug directory.
use object::pe::{self, ImageDataDirectory, ImageDebugDirectory, ImageDosHeader, ImageNtHeaders32};
use object::read::pe::ImageNtHeaders;
use object::{pod, LittleEndian as LE};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::GetCurrentProcess;

// IMAGE_DLLCHARACTERISTICS_EX_HOTPATCH_COMPATIBLE in winnt.h.
const HOTPATCH_COMPATIBLE: u32 = 0x80;

pub(super) fn is_compatible(module: HMODULE) -> bool {
    extended_characteristics(module).is_some_and(|flags| flags & HOTPATCH_COMPATIBLE != 0)
}

fn extended_characteristics(module: HMODULE) -> Option<u32> {
    let info = super::module_info(module).ok()?;
    let read = |rva: u32, len: usize| {
        let offset = rva as usize;
        if offset.checked_add(len)? > info.SizeOfImage as usize {
            return None;
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0u8);
        // SAFETY: The destination owns len writable bytes. ReadProcessMemory checks
        // whether the source range is readable, including discardable debug records.
        unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                info.lpBaseOfDll.wrapping_byte_add(offset),
                bytes.as_mut_ptr().cast(),
                len,
                None,
            )
        }
        .ok()?;
        Some(bytes)
    };
    let dos = read(0, size_of::<ImageDosHeader>())?;
    let nt_rva = ImageDosHeader::parse(dos.as_slice())
        .ok()?
        .nt_headers_offset();
    let nt = read(
        nt_rva,
        size_of::<ImageNtHeaders32>()
            + pe::IMAGE_NUMBEROF_DIRECTORY_ENTRIES * size_of::<ImageDataDirectory>(),
    )?;
    let (_, directories) = ImageNtHeaders32::parse(nt.as_slice(), &mut 0).ok()?;
    let debug = directories.get(pe::IMAGE_DIRECTORY_ENTRY_DEBUG)?;
    let data = read(debug.virtual_address.get(LE), debug.size.get(LE) as usize)?;
    let records = pod::slice_from_all_bytes::<ImageDebugDirectory>(&data).ok()?;
    let flags = records.iter().find(|record| {
        record.typ.get(LE) == pe::IMAGE_DEBUG_TYPE_EX_DLLCHARACTERISTICS
            && record.size_of_data.get(LE) >= 4
            && record.address_of_raw_data.get(LE) != 0
    })?;
    // The debug record stores an RVA for the in-memory image and a separate file offset.
    Some(u32::from_le_bytes(
        read(flags.address_of_raw_data.get(LE), 4)?
            .try_into()
            .ok()?,
    ))
}
