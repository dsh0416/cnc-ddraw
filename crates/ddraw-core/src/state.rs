#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub refresh_rate: u32,
}

impl Default for DisplayMode {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            bpp: 32,
            refresh_rate: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DirectDrawState {
    mode: DisplayMode,
    cooperative_level: u32,
    primary: Vec<u8>,
    pitch: usize,
}

impl Default for DirectDrawState {
    fn default() -> Self {
        Self::new(DisplayMode::default())
    }
}

impl DirectDrawState {
    pub fn new(mode: DisplayMode) -> Self {
        let bytes_per_pixel = (mode.bpp.max(8) / 8) as usize;
        let pitch = mode.width as usize * bytes_per_pixel;
        let primary = vec![0; pitch * mode.height as usize];

        Self {
            mode,
            cooperative_level: 0,
            primary,
            pitch,
        }
    }

    pub fn mode(&self) -> DisplayMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: DisplayMode) {
        *self = Self::new(mode);
    }

    pub fn cooperative_level(&self) -> u32 {
        self.cooperative_level
    }

    pub fn set_cooperative_level(&mut self, flags: u32) {
        self.cooperative_level = flags;
    }

    pub fn primary(&self) -> &[u8] {
        &self.primary
    }

    pub fn primary_mut(&mut self) -> &mut [u8] {
        &mut self.primary
    }

    pub fn primary_ptr(&mut self) -> *mut u8 {
        self.primary.as_mut_ptr()
    }

    pub fn pitch(&self) -> usize {
        self.pitch
    }
}
