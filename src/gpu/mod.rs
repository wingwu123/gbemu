pub mod registers;
pub mod tiles;

use crate::cpu::EmulationMode;
use crate::gpu::registers::{ColorPalette, LcdControl, LcdPosition, LcdStatus, MonochromePalette};
use crate::gpu::tiles::{BgAttr, Sprite};
use std::collections::VecDeque;

const VRAM_BANK_SIZE: usize = 0x2000;
const OAM_SIZE: usize = 0xA0;
const PALETTE_RAM_SIZE: usize = 0x40;
const SCREEN_WIDTH: usize = 160;
const SCREEN_HEIGHT: usize = 144;
const SCREEN_DEPTH: usize = 4;
const VRAM_OFFSET: u16 = 0x8000;
const OAM_OFFSET: u16 = 0xFE00;

// macro_rules! bit {
//     ( $upper:expr , $lower:expr , $mask:expr ) => {
//         ((((($upper & $mask) != 0) as u8) << 1) | ((($lower & $mask) != 0) as u8))
//     };
// }

#[derive(Debug, PartialEq)]
pub enum GpuMode {
    OamSearch,
    PixelTransfer,
    HBlank,
    VBlank,
}

impl From<&GpuMode> for u8 {
    fn from(mode: &GpuMode) -> u8 {
        match mode {
            GpuMode::HBlank => 0,
            GpuMode::VBlank => 1,
            GpuMode::OamSearch => 2,
            GpuMode::PixelTransfer => 3,
        }
    }
}

#[derive(PartialEq)]
enum PixelType {
    BgColor0,
    BgColorOpaque,
    BgPriorityOverride,
}

pub enum FetcherState {
    Sleep(usize),
    ReadTileNumber,
    ReadTileDataLow,
    ReadTileDataHigh,
    Push(usize),
}

pub enum FetchType {
    Background,
    Window,
}

pub struct Fetcher {
    pub state: FetcherState,
    pub fetching: FetchType,
    pub x: u8,
    pub tile_num: u8,
    pub low: u8,
    pub high: u8,
}

impl Fetcher {
    pub fn new() -> Self {
        Self {
            state: FetcherState::Sleep(0),
            fetching: FetchType::Background,
            x: 0,
            tile_num: 0,
            low: 0xFF,
            high: 0xFF,
        }
    }
}

pub struct BgFifo {
    pub q: VecDeque<u8>,
    pub x: u8,
    pub scx: u8,
}

impl BgFifo {
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(16),
            x: 0,
            scx: 0,
        }
    }

    pub fn clear_fifo(&mut self) {
        self.q.clear();
    }

    pub fn size(&mut self) -> usize {
        self.q.len()
    }

    pub fn allow_push(&self) -> bool {
        self.q.len() <= 8
    }

    pub fn push(&mut self, mut low: u8, mut high: u8) {
        for _ in 0..8 {
            self.q.push_back((low >> 7) | ((high >> 7) << 1));
            low <<= 1;
            high <<= 1;
        }
    }

    pub fn pop(&mut self) -> u8 {
        self.q.pop_front().unwrap()
    }
}

pub struct Gpu {
    pub lcd: Vec<u8>,
    pub vram0: Vec<u8>,
    pub vram1: Vec<u8>,
    pub bgp_ram: Vec<u8>,
    pub obp_ram: Vec<u8>,
    cgbp: ColorPalette,
    emu_mode: EmulationMode,
    oam: Vec<u8>,
    pixel_types: Vec<PixelType>,
    lcdc: LcdControl,
    dmgp: MonochromePalette,
    position: LcdPosition,
    stat: LcdStatus,
    clock: usize,
    pub request_vblank_int: bool,
    pub request_lcd_int: bool,
    vram_bank: usize,
    win_counter: usize,
    pub oam_dma_active: bool,

    // Pixel Pipeline
    bg_fifo: BgFifo,
    fetcher: Fetcher,
    borrowed_cycles: usize,
}

impl Gpu {
    pub fn new(emu_mode: EmulationMode) -> Self {
        let mut pixel_types = vec![];
        for _ in 0..SCREEN_WIDTH {
            pixel_types.push(PixelType::BgColor0);
        }

        Gpu {
            lcd: vec![0; SCREEN_HEIGHT * SCREEN_WIDTH * SCREEN_DEPTH],
            vram0: vec![0; VRAM_BANK_SIZE],
            vram1: vec![0; VRAM_BANK_SIZE],
            bgp_ram: vec![0; PALETTE_RAM_SIZE],
            obp_ram: vec![0; PALETTE_RAM_SIZE],
            oam: vec![0; OAM_SIZE],
            cgbp: ColorPalette::default(),
            emu_mode,
            pixel_types,
            lcdc: LcdControl::default(),
            dmgp: MonochromePalette::default(),
            position: LcdPosition::default(),
            stat: LcdStatus::default(),
            clock: 0,
            request_vblank_int: false,
            request_lcd_int: false,
            vram_bank: 0,
            win_counter: 0,
            oam_dma_active: false,

            // Pixel Pipeline
            bg_fifo: BgFifo::new(),
            fetcher: Fetcher::new(),
            borrowed_cycles: 0,
        }
    }

    pub fn mode(&self) -> &GpuMode {
        &self.stat.mode
    }

    pub fn screen(&self) -> *const u8 {
        self.lcd.as_ptr()
    }

    // ----------------------------------------------------------------------
    // Pixel FIFO
    // FIFO - 4MHz
    //  pushes one pixel per clock
    //  pauses unless it contains more than 8 pixels
    //
    // Fetch - 2MHz
    //  3 clocks to fetch 8 pixels
    //  pauses in 4th clock unless space in FIFO
    //
    // FIFO and Fetcher run in parallel
    //
    // Fetcher:
    //  Read Tile #
    //  Read Data 0
    //  Read Data 1
    //
    // Fetcher idles until pixel FIFO has 8 available slots
    // Then puts 8 pixels into the FIFO
    //
    // Scrolling:
    //  If SCX = 3, FIFO simply discard the first 3 pixels,
    //  starts pushing from 4th pixel onward onto the LCD
    //
    // If FIFO has pushed 160 pixels, it discards remaining pixels
    //
    // Window:
    //  While FIFO is laying out pixels, if next pixel is a Window pixel,
    //  the FIFO is cleared, Fetcher switches over to window map,
    //  and Fetcher is restarted
    //
    // Sprites:
    //  N/A
    //
    // Extra:
    //  LIJI says: "Only the uppermost 5 bits have an effect
    //   3 bits affect the sub-tile scrolling, 5 bits affect the tile offset in the tilemap"
    //
    // ----------------------------------------------------------------------

    fn bg_fifo_tick(&mut self) {
        if self.bg_fifo.size() < 8 {
            return;
        }

        if self.bg_fifo.scx > 0 {
            self.bg_fifo.pop();
            self.bg_fifo.scx -= 1;
            self.borrowed_cycles += 1;
            return;
        }

        let value = self.bg_fifo.pop();
        let (r, g, b) = self.get_rgb(value, self.dmgp.bgp);
        self.update_screen_row(self.position.lx as usize, r, g, b);

        self.position.lx += 1;
    }

    fn fetcher_tick(&mut self) {
        match self.fetcher.state {
            FetcherState::Sleep(0) => {
                self.fetcher.state = FetcherState::ReadTileNumber;
            }
            // Read tile number from tile map
            FetcherState::ReadTileNumber => {
                match self.fetcher.fetching {
                    FetchType::Background => {
                        let base = self.lcdc.bg_tilemap();
                        let row = self.position.ly.wrapping_add(self.position.scroll_y) / 8;
                        let col = (self.position.scroll_x / 8 + self.fetcher.x) % 32;
                        self.fetcher.tile_num = self.get_byte(base + row as u16 * 32 + col as u16);
                    }
                    FetchType::Window => {}
                }
                self.fetcher.state = FetcherState::Sleep(1);
            }
            FetcherState::Sleep(1) => {
                self.fetcher.state = FetcherState::ReadTileDataLow;
            }
            // Fetch lower byte of current row from tile at tile number
            FetcherState::ReadTileDataLow => {
                let row = self.position.ly.wrapping_add(self.position.scroll_y) % 8;
                let tile_addr =
                    self.tiledata_addr(self.lcdc.bg_tiledata_sel, self.fetcher.tile_num);
                self.fetcher.low = self.get_byte(tile_addr + row as u16 * 2);
                self.fetcher.state = FetcherState::Sleep(2);
            }
            FetcherState::Sleep(2) => {
                self.fetcher.state = FetcherState::ReadTileDataHigh;
            }
            // Fetch upper byte of current row from tile at tile number
            FetcherState::ReadTileDataHigh => {
                let row = self.position.ly.wrapping_add(self.position.scroll_y) % 8;
                let tile_addr =
                    self.tiledata_addr(self.lcdc.bg_tiledata_sel, self.fetcher.tile_num);
                self.fetcher.high = self.get_byte(tile_addr + row as u16 * 2 + 1);
                self.fetcher.state = FetcherState::Push(0);
            }
            // Push tile row data to pixel FIFO
            FetcherState::Push(0) => {
                self.fetcher.x = (self.fetcher.x + 1) % 32;
                self.fetcher.state = FetcherState::Push(1);
            }
            // Push tile row data to pixel FIFO
            FetcherState::Push(1) => {
                if self.bg_fifo.allow_push() {
                    self.bg_fifo.push(self.fetcher.low, self.fetcher.high);
                    self.fetcher.state = FetcherState::Sleep(0);
                }
            }
            _ => (),
        }
    }

    fn tiledata_addr(&self, sel: u8, idx: u8) -> u16 {
        if sel == 0 {
            0x8800u16 + (idx as i8 as i16 + 128) as u16 * 16
        } else {
            0x8000u16 + (idx as u16 * 16)
        }
    }

    fn is_win_enabled(&self) -> bool {
        self.lcdc.window_enabled(&self.emu_mode)
            && (self.position.window_x < 167)
            && (self.position.window_y < 144)
    }

    #[inline]
    fn is_win_pixel(&self) -> bool {
        self.position.window_x <= (self.position.lx + 7) as u8
            && self.position.window_y <= self.position.ly
    }

    fn update_screen_row(&mut self, x: usize, r: u8, g: u8, b: u8) {
        let ly = self.position.ly as usize;
        self.lcd[ly * SCREEN_WIDTH * SCREEN_DEPTH + x * SCREEN_DEPTH + 0] = r;
        self.lcd[ly * SCREEN_WIDTH * SCREEN_DEPTH + x * SCREEN_DEPTH + 1] = g;
        self.lcd[ly * SCREEN_WIDTH * SCREEN_DEPTH + x * SCREEN_DEPTH + 2] = b;
        self.lcd[ly * SCREEN_WIDTH * SCREEN_DEPTH + x * SCREEN_DEPTH + 3] = 255;
    }

    fn get_rgb(&self, value: u8, palette: u8) -> (u8, u8, u8) {
        match (palette >> (2 * value)) & 0x03 {
            0 => (224, 247, 208),
            1 => (136, 192, 112),
            2 => (52, 104, 86),
            _ => (8, 23, 33),
        }
    }

    fn get_rgb_cgb(&self, color_num: u8, palette_num: usize, obp: bool) -> (u8, u8, u8) {
        let palette_idx = palette_num * 8;
        let color_idx = palette_idx + color_num as usize * 2;

        let palette = if obp {
            (self.obp_ram[color_idx + 1] as u16) << 8 | self.obp_ram[color_idx + 0] as u16
        } else {
            (self.bgp_ram[color_idx + 1] as u16) << 8 | self.bgp_ram[color_idx + 0] as u16
        };

        self.color_correct(
            (palette & 0x001F) >> 0,
            (palette & 0x03E0) >> 5,
            (palette & 0x7C00) >> 10,
        )
    }

    fn color_correct(&self, r: u16, g: u16, b: u16) -> (u8, u8, u8) {
        (
            ((r << 3) | (r >> 2)) as u8,
            ((g << 3) | (g >> 2)) as u8,
            ((b << 3) | (b >> 2)) as u8,
        )
    }

    pub fn tick(&mut self, mut cycles: usize) {
        if self.lcdc.display_enable == 0 {
            return;
        }

        while cycles > 0 {
            match self.stat.mode {
                GpuMode::OamSearch => cycles = self.oam_search_tick(cycles),
                GpuMode::PixelTransfer => cycles = self.pixel_transfer_tick(cycles),
                GpuMode::HBlank => cycles = self.hblank_tick(cycles),
                GpuMode::VBlank => cycles = self.vblank_tick(cycles),
            }
        }
    }

    fn oam_search_tick(&mut self, cycles: usize) -> usize {
        if self.clock + cycles >= 80 {
            let cycles_left = self.clock + cycles - 80;
            self.clock = 0;
            self.change_mode(GpuMode::PixelTransfer);
            cycles_left
        } else {
            self.clock += cycles;
            0
        }
    }

    fn pixel_transfer_tick(&mut self, mut cycles: usize) -> usize {
        while cycles > 0 && (self.position.lx as usize) < SCREEN_WIDTH {
            self.fetcher_tick();
            self.bg_fifo_tick();
            cycles -= 1
        }

        if (self.position.lx as usize) >= SCREEN_WIDTH {
            self.change_mode(GpuMode::HBlank);
        }

        cycles
    }

    fn hblank_tick(&mut self, cycles: usize) -> usize {
        if self.clock + cycles >= 204 - self.borrowed_cycles {
            let cycles_left = self.clock + cycles - (204 - self.borrowed_cycles);
            self.clock = 0;
            self.position.ly += 1;
            self.check_coincidence();

            if self.position.ly > 143 {
                self.change_mode(GpuMode::VBlank);
                self.request_vblank_interrupt();
            } else {
                self.change_mode(GpuMode::OamSearch);
            }

            cycles_left
        } else {
            self.clock += cycles;
            0
        }
    }

    fn vblank_tick(&mut self, cycles: usize) -> usize {
        if self.clock + cycles >= 456 {
            let cycles_left = self.clock + cycles - 456;
            self.clock = 0;
            self.position.ly += 1;

            // STRANGE BEHAVIOR
            if self.position.ly == 153 {
                self.position.ly = 0;
                self.check_coincidence();
            }

            if self.position.ly == 1 {
                self.position.ly = 0;
                self.change_mode(GpuMode::OamSearch);
            }

            cycles_left
        } else {
            self.clock += cycles;
            0
        }
    }

    // pub fn tick2(&mut self, cycles: usize) {
    //     if self.lcdc.display_enable == 0 {
    //         return;
    //     }

    //     match self.stat.mode {
    //         GpuMode::OamSearch => {
    //             self.clock += cycles;

    //             // 80 clocks
    //             if self.clock >= 80 {
    //                 self.clock -= 80;
    //                 self.change_mode(GpuMode::PixelTransfer);
    //             }
    //         }
    //         GpuMode::PixelTransfer => {
    //             self.clock += cycles;
    //             for _ in 0..cycles {
    //                 self.bg_fifo_tick();
    //                 self.fetcher_tick();
    //             }
    //         }
    //         GpuMode::HBlank => {
    //             self.clock += cycles;

    //             // 204 clocks
    //             if self.clock >= 204 {
    //                 self.clock -= 204;
    //                 self.position.ly += 1;
    //                 self.check_coincidence();

    //                 if self.position.ly > 143 {
    //                     self.change_mode(GpuMode::VBlank);
    //                     self.request_vblank_interrupt();
    //                 } else {
    //                     self.change_mode(GpuMode::OamSearch);
    //                 }
    //             }
    //         }
    //         GpuMode::VBlank => {
    //             self.clock += cycles;

    //             // 4560 clocks, 10 lines
    //             if self.clock >= 456 {
    //                 self.clock -= 456;
    //                 self.position.ly += 1;
    //                 self.check_coincidence();

    //                 // STRANGE BEHAVIOR: At line 153, V-Blank has already reached
    //                 // the top of the screen and is to be treated like line 0.
    //                 if self.position.ly == 153 {
    //                     self.position.ly = 0;
    //                     self.check_coincidence();
    //                 }

    //                 if self.position.ly == 1 {
    //                     self.position.ly = 0;
    //                     self.win_counter = 0;
    //                     self.change_mode(GpuMode::OamSearch);
    //                 }
    //             }
    //         }
    //     }
    // }

    fn check_coincidence(&mut self) {
        if self.position.ly == self.position.lyc {
            self.stat.coincident = 0x04;
            if self.stat.lyc_int != 0 {
                self.request_lcd_interrupt();
            }
        }
    }

    fn change_mode(&mut self, mode: GpuMode) {
        self.stat.mode = mode;
        match self.stat.mode {
            GpuMode::OamSearch if self.stat.oam_int != 0 => self.request_lcd_interrupt(),
            GpuMode::HBlank if self.stat.hblank_int != 0 => self.request_lcd_interrupt(),
            GpuMode::VBlank if self.stat.vblank_int != 0 => self.request_lcd_interrupt(),
            _ => (),
        }
    }

    #[inline]
    fn request_lcd_interrupt(&mut self) {
        self.request_lcd_int = true;
    }

    fn clear_screen(&mut self) {
        for i in 0..self.lcd.len() {
            self.lcd[i] = 255;
        }
    }

    pub fn set_byte(&mut self, addr: u16, value: u8) {
        match addr {
            0x8000..=0x9FFF => match self.stat.mode {
                GpuMode::PixelTransfer if self.lcdc.display_enabled() => (),
                _ => self.set_vram_byte(addr, value, self.vram_bank),
            },
            0xFE00..=0xFE9F => match self.stat.mode {
                GpuMode::OamSearch | GpuMode::PixelTransfer if self.lcdc.display_enabled() => (),
                _ => self.oam[(addr - OAM_OFFSET) as usize] = value,
            },
            0xFF40 => {
                let old_display_enable = self.lcdc.display_enable;
                self.lcdc.display_enable = value & 0x80;
                if old_display_enable != 0 && self.lcdc.display_enable == 0 {
                    self.change_mode(GpuMode::HBlank);
                    // self.stat.mode = GpuMode::HBlank;
                    self.position.ly = 0;
                    self.win_counter = 0;
                    self.clock = 0;
                    self.clear_screen();
                }
                self.lcdc.win_tilemap_sel = value & 0x40;
                self.lcdc.win_display_enable = value & 0x20;
                self.lcdc.bg_tiledata_sel = value & 0x10;
                self.lcdc.bg_tilemap_sel = value & 0x08;
                self.lcdc.obj_size = value & 0x04;
                self.lcdc.obj_display_enable = value & 0x02;
                self.lcdc.lcdc0 = value & 0x01;
            }
            0xFF41 => {
                self.stat.lyc_int = value & 0x40;
                self.stat.oam_int = value & 0x20;
                self.stat.vblank_int = value & 0x10;
                self.stat.hblank_int = value & 0x08;
            }
            0xFF42 => self.position.scroll_y = value,
            0xFF43 => self.position.scroll_x = value,
            0xFF44 => (),
            0xFF45 => self.position.lyc = value,
            0xFF47 => self.dmgp.bgp = value,
            0xFF48 => self.dmgp.obp0 = value,
            0xFF49 => self.dmgp.obp1 = value,
            0xFF4A => self.position.window_y = value,
            0xFF4B => self.position.window_x = value,
            0xFF4F => self.vram_bank = (value & 0x01) as usize,
            0xFF68 => {
                self.cgbp.bgp_idx = value & 0x3F;
                self.cgbp.bgp_auto_incr = (value & 0x80) != 0;
            }
            0xFF69 => {
                if self.stat.mode != GpuMode::PixelTransfer {
                    self.bgp_ram[self.cgbp.bgp_idx as usize] = value;
                }
                if self.cgbp.bgp_auto_incr {
                    self.cgbp.bgp_idx = (self.cgbp.bgp_idx + 1) % 0x40;
                }
            }
            0xFF6A => {
                self.cgbp.obp_idx = value & 0x3F;
                self.cgbp.obp_auto_incr = (value & 0x80) != 0;
            }
            0xFF6B => {
                if self.stat.mode != GpuMode::PixelTransfer {
                    self.obp_ram[self.cgbp.obp_idx as usize] = value;
                }
                if self.cgbp.obp_auto_incr {
                    self.cgbp.obp_idx = (self.cgbp.obp_idx + 1) % 0x40;
                }
            }
            _ => panic!("Unexpected addr in gpu.set_byte {:#X}", addr),
        }
    }

    pub fn get_byte(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0x9FFF => match self.stat.mode {
                GpuMode::PixelTransfer if !self.oam_dma_active => 0x00,
                _ => self.get_vram_byte(addr, self.vram_bank),
            },
            0xFE00..=0xFE9F => match self.stat.mode {
                GpuMode::OamSearch | GpuMode::PixelTransfer if !self.oam_dma_active => 0x00,
                _ => self.oam[(addr - OAM_OFFSET) as usize],
            },
            0xFF40 => u8::from(&self.lcdc),
            0xFF41 => u8::from(&self.stat),
            0xFF42 => self.position.scroll_y,
            0xFF43 => self.position.scroll_x,
            0xFF44 => self.position.ly,
            0xFF45 => self.position.lyc,
            // Write only register FF46
            0xFF46 => 0xFF,
            0xFF47 => self.dmgp.bgp,
            0xFF48 => self.dmgp.obp0,
            0xFF49 => self.dmgp.obp1,
            0xFF4A => self.position.window_y,
            0xFF4B => self.position.window_x,
            0xFF4F => 0xFE | self.vram_bank as u8,
            0xFF68 if self.emu_mode == EmulationMode::Cgb => self.cgbp.bgp(),
            0xFF69 if self.emu_mode == EmulationMode::Cgb => {
                self.bgp_ram[self.cgbp.bgp_idx as usize]
            }
            0xFF6A if self.emu_mode == EmulationMode::Cgb => self.cgbp.obp(),
            0xFF6B if self.emu_mode == EmulationMode::Cgb => {
                self.obp_ram[self.cgbp.obp_idx as usize]
            }
            _ => panic!("Unexpected addr in gpu.get_byte {:#X}", addr),
        }
    }

    fn set_vram_byte(&mut self, addr: u16, value: u8, bank: usize) {
        match addr {
            0x8000..=0x9FFF => {
                if bank == 0 {
                    self.vram0[(addr - VRAM_OFFSET) as usize] = value;
                } else {
                    self.vram1[(addr - VRAM_OFFSET) as usize] = value;
                }
            }
            _ => panic!("Unexpected addr in get_vram_byte"),
        }
    }

    fn get_vram_byte(&self, addr: u16, bank: usize) -> u8 {
        match addr {
            0x8000..=0x9FFF => {
                if bank == 0 {
                    self.vram0[(addr - VRAM_OFFSET) as usize]
                } else {
                    self.vram1[(addr - VRAM_OFFSET) as usize]
                }
            }
            _ => panic!("Unexpected addr in get_vram_byte"),
        }
    }

    #[inline]
    fn request_vblank_interrupt(&mut self) {
        self.request_vblank_int = true;
    }
}
