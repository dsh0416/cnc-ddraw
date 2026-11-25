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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::WS_POPUP;

    const DS_SETFONT_U32: u32 = 0x40;

    fn push_utf16z(buf: &mut Vec<u8>, s: &str) {
        for cu in s.encode_utf16() {
            buf.extend_from_slice(&cu.to_le_bytes());
        }
        buf.extend_from_slice(&0u16.to_le_bytes());
    }

    pub(in crate::window) fn build_standard_template(
        cx: i16,
        cy: i16,
        pt: u16,
        with_item: bool,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let style = WS_POPUP.0 | DS_SETFONT_U32;
        buf.extend_from_slice(&style.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // dwExtendedStyle
        buf.extend_from_slice(&(u16::from(with_item)).to_le_bytes()); // cdit
        buf.extend_from_slice(&0i16.to_le_bytes()); // x
        buf.extend_from_slice(&0i16.to_le_bytes()); // y
        buf.extend_from_slice(&cx.to_le_bytes());
        buf.extend_from_slice(&cy.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // menu
        buf.extend_from_slice(&0u16.to_le_bytes()); // class
        buf.extend_from_slice(&0u16.to_le_bytes()); // title
        buf.extend_from_slice(&pt.to_le_bytes());
        push_utf16z(&mut buf, "MS Sans Serif");
        if with_item {
            while buf.len() % 4 != 0 {
                buf.push(0);
            }
            buf.extend_from_slice(&0x5000_0000u32.to_le_bytes()); // WS_CHILD | WS_VISIBLE
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&10i16.to_le_bytes());
            buf.extend_from_slice(&10i16.to_le_bytes());
            buf.extend_from_slice(&50i16.to_le_bytes());
            buf.extend_from_slice(&14i16.to_le_bytes());
            buf.extend_from_slice(&100u16.to_le_bytes()); // id
            buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
            buf.extend_from_slice(&0x0082u16.to_le_bytes()); // Static
            push_utf16z(&mut buf, "OK");
            buf.extend_from_slice(&0u16.to_le_bytes()); // creation data
        }
        buf
    }

    pub(in crate::window) fn build_ex_template(pt: u16) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u16.to_le_bytes()); // dlgVer
        buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // signature
        buf.extend_from_slice(&0u32.to_le_bytes()); // helpID
        buf.extend_from_slice(&0u32.to_le_bytes()); // exStyle
        buf.extend_from_slice(&(WS_POPUP.0 | DS_SETFONT_U32).to_le_bytes()); // style
        buf.extend_from_slice(&0u16.to_le_bytes()); // cDlgItems
        buf.extend_from_slice(&0i16.to_le_bytes()); // x
        buf.extend_from_slice(&0i16.to_le_bytes()); // y
        buf.extend_from_slice(&200i16.to_le_bytes()); // cx
        buf.extend_from_slice(&100i16.to_le_bytes()); // cy
        buf.extend_from_slice(&0u16.to_le_bytes()); // menu
        buf.extend_from_slice(&0u16.to_le_bytes()); // class
        push_utf16z(&mut buf, "Ex"); // title
        buf.extend_from_slice(&pt.to_le_bytes());
        buf.extend_from_slice(&400u16.to_le_bytes()); // weight
        buf.push(0); // italic
        buf.push(1); // charset
        push_utf16z(&mut buf, "MS Shell Dlg");
        buf
    }

    #[test]
    fn dialog_template_point_size_scaling() {
        let std_tpl = build_standard_template(200, 100, 8, true);
        assert!(FontPatch::from_bytes(&std_tpl, 96).is_none());
        for (template, offset) in [(&std_tpl, 24), (&build_ex_template(8), 36)] {
            for (dpi, expected) in [(192, 4), (240, 3)] {
                let patch = FontPatch::from_bytes(template, dpi).unwrap();
                assert_eq!(patch.len, template.len());
                assert_eq!(patch.offset, offset);
                assert_eq!(patch.point_size, expected);
            }
        }
    }

    #[test]
    fn truncated_templates_are_rejected() {
        for template in [
            build_standard_template(200, 100, 8, true),
            build_ex_template(8),
        ] {
            for len in 0..template.len() {
                assert!(
                    FontPatch::from_bytes(&template[..len], 192).is_none(),
                    "accepted {len} bytes"
                );
            }
        }
    }

    #[test]
    fn control_creation_data_lengths_follow_each_format() {
        let mut standard = build_standard_template(200, 100, 8, true);
        let end = standard.len();
        standard[end - 2..].copy_from_slice(&5u16.to_le_bytes());
        standard.extend_from_slice(&[1, 2, 3]);
        assert!(FontPatch::parse(None, 192, |i| u16::from_le_bytes([
            standard[i],
            standard[i + 1]
        ]))
        .is_none());

        let mut extended = build_ex_template(8);
        extended[16..18].copy_from_slice(&1u16.to_le_bytes());
        extended.resize(extended.len().next_multiple_of(4) + 24, 0);
        extended.extend_from_slice(&[0xff, 0xff, 0x82, 0, 0, 0, 3, 0, 1, 2, 3]);

        for template in [standard, extended] {
            assert_eq!(
                FontPatch::from_bytes(&template, 192).unwrap().len,
                template.len()
            );
            assert!(FontPatch::from_bytes(&template[..template.len() - 1], 192).is_none());
        }
    }

    #[test]
    fn system_message_font_can_omit_typeface() {
        let mut standard = build_standard_template(200, 100, 0x7fff, false);
        standard.truncate(26); // template ends immediately after pointsize
        assert!(FontPatch::parse(None, 192, |i| u16::from_le_bytes([
            standard[i],
            standard[i + 1]
        ]))
        .is_none());
    }
}
