/// boot.rs - Boot information structures for FreyaOS.
/// 
/// The stage2 bootloader fills in the E820 memory map at physical address
/// 0x0500 (count as u16) and 0x0504 (entries, 24 bytes each) before entering
/// long mode. The kernel reads those directly via 'e820' module below.
/// 
/// UEFI structures are defined here for future use but are not wired up yet.
/// The BIOS path is the active one.

/// -----------------------------------------------------------------
/// Boot mode
/// -----------------------------------------------------------------

/// Which firmware handed us control.
//#[repr(u32)]
//#[derive(Debug, Clone, Copy)]
//pub enum BootMode {
//    Bios = 0,
//    // Uefi = 1,
//}

/// -----------------------------------------------------------------
/// E820 memory map (BIOS path - populated by stage2 at 0x0500)
/// -----------------------------------------------------------------
pub const E820_COUNT_ADDR: usize = 0x0500;

/// Physical address where stage2 stared the E820 entries (24 bytes each).
pub const E820_BUFFER_ADDR: usize = 0x0504;

/// Maximum entries we asked stage2 to collect.
pub const E820_MAX_ENTRIES: usize = 32;

/// A single E820 memory map entry as written by the BIOS (INT 15h / AX=E820h).
/// 24 bytes with the ACPI 3.0 extended-attributes field included.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct E820Entry {

    // Physical start address of the region.
    pub base: u64,
    
    // Length of the region in bytes.
    pub length: u64,

    // Region type (1 = usable RAM, 2 = reserved, 3 = ACPI reclaimable,
    // 4 = ACPI NVS, 5 = bad memory).
    pub region_type: u32,

    // ACPI 3.0 extended attributes (bit 0 = entry valid; ignore if BIOS
    // wrote only 20 bytes).
    pub acpi_attrs: u32,
}

impl E820Entry {

    // Returns true if this region is usable RAM (type 1) and non-zero length.
    #[inline]
    pub fn is_usable(&self) -> bool {
        self.region_type == 1 && self.length > 0
    }
}

/// Where stage2 records FLAGS as the BIOS's disk and memory-map calls left
/// them (Entry 16; before the display is set up since Entry 19).
pub const BIOS_FLAGS_ADDR: usize = 0x04F0;

extern "C" {
    /// RFLAGS when the kernel's _start was entered (entry.asm).
    static entry_rflags: u64;
}

const IF: u64 = 1 << 9;

/// The BIOS returned from stage2's disk and memory-map calls with
/// interrupts enabled.
pub fn bios_left_interrupts_on() -> bool {
    unsafe { (BIOS_FLAGS_ADDR as *const u16).read_volatile() as u64 & IF != 0 }
}

/// Interrupts were enabled when the kernel was entered.
pub fn entered_with_interrupts_on() -> bool {
    unsafe { core::ptr::read_volatile(&raw const entry_rflags) & IF != 0 }
}

/// What stage 2 did about the display (Entry 18), as it left it at 0x0900.
/// Layout: bootloader/BIOS/stage2.asm, DI_* constants.
#[derive(Clone, Copy)]
pub struct DisplayInfo {
    /// 0 graphics mode set; text mode because: 1 built with TEXT_MODE=1,
    /// 2 Shift held, 3 no VBE 2.0, 4 no usable mode, 5 the mode set failed,
    /// 6 the ROM font couldn't be copied.
    pub status: u8,
    /// The VGA ROM's 8x16 font is at FONT_ADDR.
    pub font: bool,
    pub mode: u16,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    /// (size, position) of each colour field in a pixel.
    pub red: (u8, u8),
    pub green: (u8, u8),
    pub blue: (u8, u8),
    pub framebuffer: u64,
    /// The display's own resolution from its EDID; (0, 0) if unknown.
    pub native: (u32, u32),
    pub vbe_version: u16,
    pub modes_listed: u32,
}

pub const DISPLAY_INFO_ADDR: usize = 0x0900;
/// 256 glyphs, 16 bytes each (one byte per row, bit 7 = leftmost pixel).
pub const FONT_ADDR: usize = 0x1000;
const DISPLAY_MAGIC: u32 = u32::from_le_bytes(*b"FDSP");

/// The display hand-off, or None if stage 2 didn't write one.
pub fn display_info() -> Option<DisplayInfo> {
    let b = |off: usize| unsafe { ((DISPLAY_INFO_ADDR + off) as *const u8).read_volatile() };
    let w = |off: usize| b(off) as u32 | (b(off + 1) as u32) << 8;
    let d = |off: usize| w(off) | w(off + 2) << 16;
    if d(0) != DISPLAY_MAGIC {
        return None;
    }
    Some(DisplayInfo {
        status: b(4),
        font: b(5) == 1,
        mode: w(6) as u16,
        width: w(8),
        height: w(10),
        pitch: w(12),
        bpp: b(14) as u32,
        red: (b(15), b(16)),
        green: (b(17), b(18)),
        blue: (b(19), b(20)),
        framebuffer: d(24) as u64,
        native: (w(28), w(30)),
        vbe_version: w(32) as u16,
        modes_listed: w(34),
    })
}

/// Read the E820 map left by stage2
/// 
/// # Safety
/// Reads from fixed physical addresses (0x0500 / 0x0504). Safe only after
/// stage2 has run and before anything overwrites that region. Call this
/// exactly once, early in '_start', before the frame allocator is initialised.
pub unsafe fn read_e820_map() -> &'static [E820Entry] {

    let count = *(E820_COUNT_ADDR as *const u16) as usize;
    let count = count.min(E820_MAX_ENTRIES); // clamp defensively

    if count == 0 {
        return &[];
    }

    core::slice::from_raw_parts(E820_BUFFER_ADDR as *const E820Entry, count)
}
/* 
/// -----------------------------------------------------------------
/// Frame-buffer (UEFI GOP / future use)
/// -----------------------------------------------------------------
/// 
/// Pixel layout in a linear frame buffer.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {

    Rgb,
    Bgr,
    Unknown,
}
 
/// Describes a linear frame buffer provided by UEFI GOP (or a VESA mode).
#[repr(C)]
pub struct FrameBufferInfo {

    pub ptr: *mut u8,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scanline: u32,
    pub format: PixelFormat,
}
 
// ---------------------------------------------------------------------------
// UEFI memory map (future use)
// ---------------------------------------------------------------------------
 
/// A single UEFI memory descriptor as returned by `GetMemoryMap`.
/// 48 bytes in the UEFI 2.x spec.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EfiMemoryDescriptor {

    pub memory_type: u32,
    pub _pad: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}
 
/// Memory map wrapper (used only on the UEFI path, not yet implemented).
#[repr(C)]
pub struct MemoryMap {

    /// Pointer to an array of `EfiMemoryDescriptor` entries.
    /// Null on the BIOS path — use `read_e820_map()` instead.
    pub buffer: *const EfiMemoryDescriptor,
    pub entry_count: u32,
    pub desc_size: u32,
}
 
/// ---------------------------------------------------------------------------
/// Top-level BootInfo passed to the kernel
/// ---------------------------------------------------------------------------
/// 
/// Information the bootloader passes to the kernel.
/// Extend this as new subsystems come online.
#[repr(C)]
pub struct BootInfo {

    pub boot_mode: BootMode,
    pub memory_map: MemoryMap,          // UEFI path only
    pub framebuffer: Option<FrameBufferInfo>,
    // Future: ACPI RSDP address, SMBIOS address, command-line string ptr, …
}
 
/// Global BootInfo instance.  Stage2 (BIOS) does not fill this in — the
/// kernel reads E820 directly via `read_e820_map()`.  A UEFI loader would
/// populate this before calling `_start`.
#[no_mangle]
pub static mut BOOT_INFO: BootInfo = BootInfo {

    boot_mode: BootMode::Bios,
    memory_map: MemoryMap {

        buffer: core::ptr::null(),
        entry_count: 0,
        desc_size: 0,
    },
    framebuffer: None,
};
*/