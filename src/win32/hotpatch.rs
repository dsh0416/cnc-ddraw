//! Publishes a prepared detour using the aligned entry word of a Win32 x86 hotpatch site.
use std::ffi::c_void;
use std::sync::atomic::{AtomicU16, Ordering};

use iced_x86::{Decoder, DecoderOptions, FlowControl};
use parking_lot::Mutex;
use windows::core::{Error, Result};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Memory::{
    VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::GetCurrentProcess;

const PADDING_LEN: usize = 5;
// retour's x86 decoder reads a five-byte jump margin plus a maximum-length instruction.
const DECODE_WINDOW: usize = PADDING_LEN + 15;
const JUMP_TO_PADDING: u16 = u16::from_le_bytes([0xeb, 0xf9]);

#[derive(Default)]
struct Publication {
    published: bool,
    restore_protection: Option<PAGE_PROTECTION_FLAGS>,
    flush_pending: bool,
}

pub(super) struct HotpatchSite {
    entry: usize,
    padding_bytes: [u8; PADDING_LEN],
    prefix: [u8; DECODE_WINDOW],
    prefix_len: usize,
    state: Mutex<Publication>,
}

fn supported_prefix(padding: [u8; PADDING_LEN], prefix: &[u8], module: HMODULE) -> Option<usize> {
    if padding != [0xcc; PADDING_LEN] && padding != [0x90; PADDING_LEN] {
        return None;
    }
    if !matches!(prefix, [0x8b, 0xff, ..] | [0x66, 0x90, ..])
        && !super::hotpatch_image::is_compatible(module)
    {
        return None;
    }
    let mut decoder = Decoder::new(32, prefix, DecoderOptions::NONE);
    let first = decoder.decode();
    if first.is_invalid() || first.len() < 2 {
        return None;
    }
    // USER32 forwarding thunks can start with a single six-byte indirect jump.
    if first.flow_control() == FlowControl::IndirectBranch && first.len() >= PADDING_LEN {
        return Some(first.len());
    }
    let mut instruction = first;
    loop {
        if instruction.is_invalid() || instruction.flow_control() != FlowControl::Next {
            return None;
        }
        if decoder.position() >= PADDING_LEN {
            return Some(decoder.position());
        }
        instruction = decoder.decode();
    }
}

impl HotpatchSite {
    /// # Safety
    /// The address must be a Win32 SDK export in an image that stays loaded for the
    /// site's lifetime. The caller owns hook installation at that address.
    pub(super) unsafe fn inspect(entry: usize) -> Result<Option<Self>> {
        let (Some(pad), Some(end)) = (
            entry.checked_sub(PADDING_LEN),
            entry.checked_add(DECODE_WINDOW),
        ) else {
            return Ok(None);
        };
        if !entry.is_multiple_of(align_of::<AtomicU16>()) {
            return Ok(None);
        }
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: VirtualQuery examines this process's mappings and fills a sized local buffer.
        let size = unsafe {
            VirtualQuery(
                Some(pad as *const c_void),
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if size == 0 {
            return Err(Error::from_thread());
        }
        if size != size_of::<MEMORY_BASIC_INFORMATION>()
            || info.State != MEM_COMMIT
            || info.Type != MEM_IMAGE
            || !matches!(
                info.Protect,
                PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
            )
            || (info.BaseAddress as usize)
                .checked_add(info.RegionSize)
                .is_none_or(|region_end| end > region_end)
        {
            return Ok(None);
        }
        // SAFETY: The entire checked range is in the live image's readable executable
        // mapping. These arrays have byte alignment and are copied before any writes.
        let (padding_bytes, prefix) = unsafe {
            (
                (pad as *const [u8; PADDING_LEN]).read(),
                (entry as *const [u8; DECODE_WINDOW]).read(),
            )
        };
        let Some(prefix_len) =
            supported_prefix(padding_bytes, &prefix, HMODULE(info.AllocationBase))
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            entry,
            padding_bytes,
            prefix,
            prefix_len,
            state: Mutex::default(),
        }))
    }

    pub(super) fn padding(&self) -> usize {
        self.entry - PADDING_LEN
    }

    pub(super) fn relocated_end(&self) -> usize {
        self.entry + self.prefix_len
    }

    fn live_padding(&self) -> [u8; PADDING_LEN] {
        // SAFETY: The inspected executable image remains mapped for the site's lifetime.
        unsafe { (self.padding() as *const [u8; PADDING_LEN]).read_volatile() }
    }

    fn entry_word(&self, published: bool) -> u16 {
        if published {
            JUMP_TO_PADDING
        } else {
            u16::from_le_bytes([self.prefix[0], self.prefix[1]])
        }
    }

    fn verify_prefix(&self, published: bool) -> Result<()> {
        // SAFETY: inspect verified the full decoder window of the permanent image.
        let live = unsafe { (self.entry as *const [u8; DECODE_WINDOW]).read_volatile() };
        if live[..2] == self.entry_word(published).to_le_bytes()
            && live[2..self.prefix_len] == self.prefix[2..self.prefix_len]
        {
            Ok(())
        } else {
            Err(Error::new(E_FAIL, "Win32 hotpatch entry changed"))
        }
    }

    pub(super) fn verify_padding(&self) -> Result<()> {
        if self.live_padding() != self.padding_bytes {
            return Err(Error::new(E_FAIL, "Win32 hotpatch padding changed"));
        }
        self.verify_prefix(false)
    }

    pub(super) fn verify_prepared(&self, dispatch: usize) -> Result<()> {
        // On i686 retour emits a direct rel32 jump ending exactly at the public entry.
        let [opcode, a, b, c, d] = self.live_padding();
        let offset = u32::from_le_bytes([a, b, c, d]) as usize;
        if opcode != 0xe9 || self.entry.wrapping_add(offset) != dispatch {
            return Err(Error::new(
                E_FAIL,
                "Win32 hotpatch padding does not target its dispatcher",
            ));
        }
        self.verify_prefix(self.state.lock().published)
    }

    /// # Safety
    /// The padding must hold a prepared, flushed detour which remains callable by
    /// any thread that may already have reached it, including after rollback.
    pub(super) unsafe fn publish(&self) -> Result<()> {
        self.transition(true)
    }

    pub(super) fn restore(&self) -> Result<()> {
        self.transition(false)
    }

    fn transition(&self, enabled: bool) -> Result<()> {
        let mut state = self.state.lock();
        self.finish_write(&mut state)?;
        if state.published == enabled {
            return if enabled {
                self.verify_prefix(true)
            } else {
                Ok(())
            };
        }
        let entry = self.entry as *mut u16;
        let mut previous = PAGE_PROTECTION_FLAGS::default();
        // SAFETY: The aligned two-byte entry occupies a single page of the live image.
        unsafe { VirtualProtect(entry.cast(), 2, PAGE_EXECUTE_READWRITE, &mut previous)? };
        state.restore_protection = Some(previous);
        // SAFETY: The aligned word is writable for this operation. The compare-exchange
        // changes the whole instruction prefix atomically and rejects a changed entry.
        let swapped = unsafe { AtomicU16::from_ptr(entry) }.compare_exchange(
            self.entry_word(state.published),
            self.entry_word(enabled),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        if swapped.is_ok() {
            state.published = enabled;
            state.flush_pending = true;
        }
        self.finish_write(&mut state)?;
        swapped
            .map(|_| ())
            .map_err(|_| Error::new(E_FAIL, "Win32 hotpatch entry changed"))
    }

    fn finish_write(&self, state: &mut Publication) -> Result<()> {
        let entry = self.entry as *const c_void;
        let mut discarded = PAGE_PROTECTION_FLAGS::default();
        let restore = if let Some(protection) = state.restore_protection {
            // SAFETY: Restore exactly the protection returned for this word, retrying on failure.
            unsafe { VirtualProtect(entry, 2, protection, &mut discarded) }
                .inspect(|_| state.restore_protection = None)
        } else {
            Ok(())
        };
        let flush = if state.flush_pending {
            // SAFETY: The two updated bytes belong to this process's live image.
            unsafe { FlushInstructionCache(GetCurrentProcess(), Some(entry), 2) }
                .inspect(|_| state.flush_pending = false)
        } else {
            Ok(())
        };
        restore.and(flush)
    }
}
