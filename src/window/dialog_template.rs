//! Read the layout of standard and extended Win32 dialog templates.
#![forbid(unsafe_code)]

const DS_SETFONT: u16 = 0x40;

pub(super) struct FontPatch {
    pub len: usize,
    pub offset: usize,
    pub point_size: u16,
}

struct Cursor<F> {
    word_at: F,
    offset: usize,
    limit: usize,
}

impl<F: Fn(usize) -> u16> Cursor<F> {
    fn skip(&mut self, n: usize) -> Option<()> {
        self.offset = self.offset.checked_add(n)?;
        (self.offset <= self.limit).then_some(())
    }

    fn word(&mut self) -> Option<u16> {
        let at = self.offset;
        self.skip(2)?;
        Some((self.word_at)(at))
    }

    fn name(&mut self, allow_ordinal: bool) -> Option<()> {
        let mut ch = self.word()?;
        if allow_ordinal && ch == 0xffff {
            return self.skip(2);
        }
        while ch != 0 {
            ch = self.word()?;
        }
        Some(())
    }
}

impl FontPatch {
    pub(super) fn from_bytes(bytes: &[u8], dpi: u32) -> Option<Self> {
        Self::parse(Some(bytes.len()), dpi, |i| {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        })
    }

    /// Only requests 16-bit fields described by the template. Padding and
    /// application-specific control creation data are skipped without reading.
    pub(super) fn parse(
        len: Option<usize>,
        dpi: u32,
        word_at: impl Fn(usize) -> u16,
    ) -> Option<Self> {
        if dpi <= 96 {
            return None;
        }
        let mut r = Cursor {
            word_at,
            offset: 0,
            limit: len.unwrap_or(isize::MAX as usize),
        };
        let first = r.word()?;
        let extended = r.word()? == 0xffff && first == 1;
        let style = if extended {
            r.skip(8)?; // helpID and exStyle
            let style = r.word()?;
            r.skip(2)?; // high word of style
            style
        } else {
            r.skip(4)?; // dwExtendedStyle
            first
        };
        if style & DS_SETFONT == 0 {
            return None;
        }
        let count = r.word()?;
        r.skip(8)?; // x, y, cx, cy
        r.name(true)?; // menu
        r.name(true)?; // class
        r.name(false)?; // title
        let offset = r.offset;
        let old_size = r.word()?;
        let point_size = (u32::from(old_size) * 96 / dpi).max(1) as u16;
        // 0x7fff requests the system message font and may omit the font fields.
        if old_size == 0 || old_size == 0x7fff || point_size == old_size {
            return None;
        }
        if extended {
            r.skip(4)?; // weight, italic, charset
        }
        r.name(false)?; // typeface
        for _ in 0..count {
            r.skip((4 - r.offset % 4) % 4)?; // each control starts on a DWORD
            r.skip(if extended { 24 } else { 18 })?;
            r.name(true)?; // class
            r.name(true)?; // title
            let size = usize::from(r.word()?);
            // Wine advances past an extra word for standard templates with
            // creation data. Without a resource length we cannot copy one
            // buffer that is known to be valid for both interpretations.
            if len.is_none() && !extended && size != 0 {
                return None;
            }
            // DLGITEMTEMPLATE includes the count word in nonzero creation-data
            // sizes. DLGITEMTEMPLATEEX counts only the bytes following it.
            let payload = if extended || size == 0 {
                size
            } else {
                size.checked_sub(2)?
            };
            r.skip(payload)?;
        }
        Some(Self {
            len: r.offset,
            offset,
            point_size,
        })
    }
}
