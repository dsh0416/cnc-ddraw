use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigValueKind {
    Bool,
    Integer,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigKey {
    pub ini_key: &'static str,
    pub field: &'static str,
    pub kind: ConfigValueKind,
    pub default_value: &'static str,
}

pub const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey {
        ini_key: "width",
        field: "window_rect.right",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "height",
        field: "window_rect.bottom",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "fullscreen",
        field: "fullscreen",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "windowed",
        field: "windowed",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "maintas",
        field: "maintas",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "aspect_ratio",
        field: "aspect_ratio",
        kind: ConfigValueKind::String,
        default_value: "",
    },
    ConfigKey {
        ini_key: "boxing",
        field: "boxing",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "maxfps",
        field: "maxfps",
        kind: ConfigValueKind::Integer,
        default_value: "-1",
    },
    ConfigKey {
        ini_key: "vsync",
        field: "vsync",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "adjmouse",
        field: "adjmouse",
        kind: ConfigValueKind::Bool,
        default_value: "true",
    },
    ConfigKey {
        ini_key: "shader",
        field: "shader",
        kind: ConfigValueKind::String,
        default_value: "Shaders\\interpolation\\catmull-rom-bilinear.glsl",
    },
    ConfigKey {
        ini_key: "posX",
        field: "window_rect.left",
        kind: ConfigValueKind::Integer,
        default_value: "-32000",
    },
    ConfigKey {
        ini_key: "posY",
        field: "window_rect.top",
        kind: ConfigValueKind::Integer,
        default_value: "-32000",
    },
    ConfigKey {
        ini_key: "renderer",
        field: "renderer",
        kind: ConfigValueKind::String,
        default_value: "auto",
    },
    ConfigKey {
        ini_key: "devmode",
        field: "devmode",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "border",
        field: "border",
        kind: ConfigValueKind::Bool,
        default_value: "true",
    },
    ConfigKey {
        ini_key: "savesettings",
        field: "save_settings",
        kind: ConfigValueKind::Integer,
        default_value: "1",
    },
    ConfigKey {
        ini_key: "resizable",
        field: "resizable",
        kind: ConfigValueKind::Bool,
        default_value: "true",
    },
    ConfigKey {
        ini_key: "d3d9_filter",
        field: "d3d9_filter",
        kind: ConfigValueKind::Integer,
        default_value: "2",
    },
    ConfigKey {
        ini_key: "anti_aliased_fonts_min_size",
        field: "anti_aliased_fonts_min_size",
        kind: ConfigValueKind::Integer,
        default_value: "13",
    },
    ConfigKey {
        ini_key: "min_font_size",
        field: "min_font_size",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "center_window",
        field: "center_window",
        kind: ConfigValueKind::Integer,
        default_value: "1",
    },
    ConfigKey {
        ini_key: "inject_resolution",
        field: "inject_resolution",
        kind: ConfigValueKind::String,
        default_value: "",
    },
    ConfigKey {
        ini_key: "vhack",
        field: "vhack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "screenshotdir",
        field: "screenshot_dir",
        kind: ConfigValueKind::String,
        default_value: ".\\Screenshots\\",
    },
    ConfigKey {
        ini_key: "toggle_borderless",
        field: "toggle_borderless",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "toggle_upscaled",
        field: "toggle_upscaled",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "noactivateapp",
        field: "noactivateapp",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "maxgameticks",
        field: "maxgameticks",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "limiter_type",
        field: "limiter_type",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "minfps",
        field: "minfps",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "nonexclusive",
        field: "nonexclusive",
        kind: ConfigValueKind::Bool,
        default_value: "true",
    },
    ConfigKey {
        ini_key: "singlecpu",
        field: "singlecpu",
        kind: ConfigValueKind::Bool,
        default_value: "true",
    },
    ConfigKey {
        ini_key: "resolutions",
        field: "resolutions",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "fixchilds",
        field: "fixchilds",
        kind: ConfigValueKind::Integer,
        default_value: "2",
    },
    ConfigKey {
        ini_key: "hook_peekmessage",
        field: "hook_peekmessage",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "fix_alt_key_stuck",
        field: "fix_alt_key_stuck",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "game_handles_close",
        field: "game_handles_close",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "fix_not_responding",
        field: "fix_not_responding",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "no_compat_warning",
        field: "no_compat_warning",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "guard_lines",
        field: "guard_lines",
        kind: ConfigValueKind::Integer,
        default_value: "200",
    },
    ConfigKey {
        ini_key: "max_resolutions",
        field: "max_resolutions",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "lock_surfaces",
        field: "lock_surfaces",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "flipclear",
        field: "flipclear",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "rgb555",
        field: "rgb555",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "no_dinput_hook",
        field: "no_dinput_hook",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "center_cursor_fix",
        field: "center_cursor_fix",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "fake_mode",
        field: "fake_mode",
        kind: ConfigValueKind::String,
        default_value: "",
    },
    ConfigKey {
        ini_key: "lock_mouse_top_left",
        field: "lock_mouse_top_left",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "win_version",
        field: "win_version",
        kind: ConfigValueKind::String,
        default_value: "",
    },
    ConfigKey {
        ini_key: "hook",
        field: "hook",
        kind: ConfigValueKind::Integer,
        default_value: "4",
    },
    ConfigKey {
        ini_key: "limit_gdi_handles",
        field: "limit_gdi_handles",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "remove_menu",
        field: "remove_menu",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "refresh_rate",
        field: "refresh_rate",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "terminate_process",
        field: "terminate_process",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "keytogglefullscreen",
        field: "hotkeys.toggle_fullscreen",
        kind: ConfigValueKind::Integer,
        default_value: "13",
    },
    ConfigKey {
        ini_key: "keytogglefullscreen2",
        field: "hotkeys.toggle_fullscreen2",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "keytogglemaximize",
        field: "hotkeys.toggle_maximize",
        kind: ConfigValueKind::Integer,
        default_value: "34",
    },
    ConfigKey {
        ini_key: "keytogglemaximize2",
        field: "hotkeys.toggle_maximize2",
        kind: ConfigValueKind::Integer,
        default_value: "0",
    },
    ConfigKey {
        ini_key: "keyunlockcursor1",
        field: "hotkeys.unlock_cursor1",
        kind: ConfigValueKind::Integer,
        default_value: "9",
    },
    ConfigKey {
        ini_key: "keyunlockcursor2",
        field: "hotkeys.unlock_cursor2",
        kind: ConfigValueKind::Integer,
        default_value: "163",
    },
    ConfigKey {
        ini_key: "keyscreenshot",
        field: "hotkeys.screenshot",
        kind: ConfigValueKind::Integer,
        default_value: "44",
    },
    ConfigKey {
        ini_key: "armadahack",
        field: "armadahack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "tshack",
        field: "tshack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "infantryhack",
        field: "infantryhack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "stronghold_hack",
        field: "stronghold_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "mgs_hack",
        field: "mgs_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "tlc_hack",
        field: "tlc_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "carma95_hack",
        field: "carma95_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "sirtech_hack",
        field: "sirtech_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "flightsim98_hack",
        field: "flightsim98_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
    ConfigKey {
        ini_key: "darkcolony_hack",
        field: "darkcolony_hack",
        kind: ConfigValueKind::Bool,
        default_value: "false",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    values: BTreeMap<&'static str, String>,
    unknown_values: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let values = CONFIG_KEYS
            .iter()
            .map(|key| (key.ini_key, key.default_value.to_owned()))
            .collect();

        Self {
            values,
            unknown_values: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn from_ini_str(input: &str) -> Self {
        let mut config = Self::default();
        let known = CONFIG_KEYS
            .iter()
            .map(|key| (key.ini_key.to_ascii_lowercase(), *key))
            .collect::<BTreeMap<_, _>>();

        for line in input.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with(';')
                || line.starts_with('#')
                || line.starts_with('[')
            {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            if let Some(schema) = known.get(&key.to_ascii_lowercase()) {
                config
                    .values
                    .insert(schema.ini_key, normalize_value(schema.kind, value));
            } else {
                config
                    .unknown_values
                    .insert(key.to_owned(), value.to_owned());
            }
        }

        if config.bool("lock_mouse_top_left") {
            config.values.insert("adjmouse", "false".to_owned());
        }

        if !config.string("aspect_ratio").is_empty() {
            config.values.insert("maintas", "true".to_owned());
        }

        config
    }

    pub fn string(&self, key: &str) -> &str {
        self.values.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn int(&self, key: &str) -> i32 {
        self.string(key).parse().unwrap_or(0)
    }

    pub fn bool(&self, key: &str) -> bool {
        matches!(
            self.string(key).to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }

    pub fn unknown_values(&self) -> &BTreeMap<String, String> {
        &self.unknown_values
    }
}

fn normalize_value(kind: ConfigValueKind, value: &str) -> String {
    match kind {
        ConfigValueKind::Bool => {
            if matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ) {
                "true".to_owned()
            } else {
                "false".to_owned()
            }
        }
        ConfigValueKind::Integer => value.parse::<i32>().unwrap_or(0).to_string(),
        ConfigValueKind::String => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_c_loader() {
        let config = Config::default();

        assert_eq!(config.int("width"), 0);
        assert_eq!(config.int("height"), 0);
        assert!(!config.bool("fullscreen"));
        assert!(!config.bool("windowed"));
        assert!(config.bool("adjmouse"));
        assert_eq!(config.string("renderer"), "auto");
        assert_eq!(config.int("hook"), 4);
        assert_eq!(config.int("d3d9_filter"), 2);
        assert_eq!(config.int("fixchilds"), 2);
    }

    #[test]
    fn parses_legacy_bool_int_and_string_values() {
        let config = Config::from_ini_str(
            r#"
            [ddraw]
            windowed=true
            renderer=direct3d9
            hook=2
            maxfps=60
            "#,
        );

        assert!(config.bool("windowed"));
        assert_eq!(config.string("renderer"), "direct3d9");
        assert_eq!(config.int("hook"), 2);
        assert_eq!(config.int("maxfps"), 60);
    }

    #[test]
    fn keeps_unknown_keys_for_deprecation_logging() {
        let config = Config::from_ini_str("custom_mod_key=value");

        assert_eq!(
            config
                .unknown_values()
                .get("custom_mod_key")
                .map(String::as_str),
            Some("value")
        );
    }

    #[test]
    fn preserves_legacy_derived_settings() {
        let config = Config::from_ini_str("aspect_ratio=16:9\nlock_mouse_top_left=true");

        assert!(config.bool("maintas"));
        assert!(!config.bool("adjmouse"));
    }
}
