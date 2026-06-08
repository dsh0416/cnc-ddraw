//! External ABI contracts that must not drift from the legacy DLL.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrdinalExport {
    pub name: &'static str,
    pub ordinal: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedExport {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub is_data: bool,
}

pub const ORDINAL_EXPORTS: &[OrdinalExport] = &[
    OrdinalExport {
        name: "AcquireDDThreadLock",
        ordinal: 1,
    },
    OrdinalExport {
        name: "CompleteCreateSysmemSurface",
        ordinal: 2,
    },
    OrdinalExport {
        name: "D3DParseUnknownCommand",
        ordinal: 3,
    },
    OrdinalExport {
        name: "DDInternalLock",
        ordinal: 5,
    },
    OrdinalExport {
        name: "DDInternalUnlock",
        ordinal: 6,
    },
    OrdinalExport {
        name: "DirectDrawCreate",
        ordinal: 8,
    },
    OrdinalExport {
        name: "DirectDrawCreateClipper",
        ordinal: 9,
    },
    OrdinalExport {
        name: "DirectDrawCreateEx",
        ordinal: 10,
    },
    OrdinalExport {
        name: "DirectDrawEnumerateA",
        ordinal: 11,
    },
    OrdinalExport {
        name: "DirectDrawEnumerateExA",
        ordinal: 12,
    },
    OrdinalExport {
        name: "DirectDrawEnumerateExW",
        ordinal: 13,
    },
    OrdinalExport {
        name: "DirectDrawEnumerateW",
        ordinal: 14,
    },
    OrdinalExport {
        name: "ReleaseDDThreadLock",
        ordinal: 21,
    },
];

pub const NAMED_EXPORTS: &[NamedExport] = &[
    NamedExport {
        name: "DirectInputCreateA",
        alias: Some("fake_DirectInputCreateA"),
        is_data: false,
    },
    NamedExport {
        name: "DirectInputCreateW",
        alias: Some("fake_DirectInputCreateW"),
        is_data: false,
    },
    NamedExport {
        name: "DirectInputCreateEx",
        alias: Some("fake_DirectInputCreateEx"),
        is_data: false,
    },
    NamedExport {
        name: "DirectInput8Create",
        alias: Some("fake_DirectInput8Create"),
        is_data: false,
    },
    NamedExport {
        name: "GameHandlesClose",
        alias: None,
        is_data: true,
    },
    NamedExport {
        name: "pvBmpBits",
        alias: Some("FakePrimarySurface"),
        is_data: true,
    },
    NamedExport {
        name: "DDIsWindowed",
        alias: None,
        is_data: false,
    },
    NamedExport {
        name: "DDGetProcAddress",
        alias: None,
        is_data: false,
    },
    NamedExport {
        name: "DDEnableZoom",
        alias: None,
        is_data: false,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    MainExecutableIat = 1,
    Inline = 2,
    LocalModulesIat = 3,
    LocalModulesIatWithProcAddress = 4,
}

impl HookMode {
    pub fn from_legacy_value(value: i32) -> Self {
        match value {
            1 => Self::MainExecutableIat,
            2 => Self::Inline,
            3 => Self::LocalModulesIat,
            _ => Self::LocalModulesIatWithProcAddress,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORTS_DEF: &str = include_str!("../../../exports.def");

    #[test]
    fn ordinal_exports_match_exports_def() {
        for expected in ORDINAL_EXPORTS {
            let needle = format!("{}{}", expected.name, " ".repeat(1));
            let line = EXPORTS_DEF
                .lines()
                .find(|line| {
                    line.trim_start().starts_with(&needle)
                        || line.trim_start().starts_with(expected.name)
                })
                .unwrap_or_else(|| panic!("missing export {}", expected.name));

            assert!(
                line.contains(&format!("@{}", expected.ordinal)),
                "{} should keep ordinal @{} in exports.def",
                expected.name,
                expected.ordinal
            );
        }
    }

    #[test]
    fn named_exports_match_exports_def() {
        for expected in NAMED_EXPORTS {
            let line = EXPORTS_DEF
                .lines()
                .find(|line| line.trim_start().starts_with(expected.name))
                .unwrap_or_else(|| panic!("missing named export {}", expected.name));

            if let Some(alias) = expected.alias {
                assert!(
                    line.contains(alias),
                    "{} should alias {}",
                    expected.name,
                    alias
                );
            }

            if expected.is_data {
                assert!(
                    line.contains("DATA"),
                    "{} should remain a DATA export",
                    expected.name
                );
            }
        }
    }

    #[test]
    fn hook_mode_values_preserve_legacy_ini_semantics() {
        assert_eq!(HookMode::from_legacy_value(1), HookMode::MainExecutableIat);
        assert_eq!(HookMode::from_legacy_value(2), HookMode::Inline);
        assert_eq!(HookMode::from_legacy_value(3), HookMode::LocalModulesIat);
        assert_eq!(
            HookMode::from_legacy_value(4),
            HookMode::LocalModulesIatWithProcAddress
        );
        assert_eq!(
            HookMode::from_legacy_value(99),
            HookMode::LocalModulesIatWithProcAddress
        );
    }
}
