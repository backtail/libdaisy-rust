//! Audio module. Handles audio startup and I/O.
//! As well as converting between the S24 input and f32 for processing.
use log::info;

use stm32h7xx_hal::{
    dma,
    gpio::{gpiob, gpioe, gpioh, Analog},
    i2c::*,
    pac, rcc,
    rcc::rec,
    sai,
    sai::*,
    stm32,
    stm32::rcc::d2ccip1r::SAI1SEL_A,
    time,
    traits::i2s::FullDuplex,
};

use cortex_m::asm;
use cortex_m::prelude::_embedded_hal_blocking_i2c_Write;
use num_enum::IntoPrimitive;

// Process samples at 1000 Hz
// With a circular buffer(*2) in stereo (*2)
pub const BLOCK_SIZE_MAX: usize = 1024;
pub const DMA_BUFFER_SIZE: usize = BLOCK_SIZE_MAX * 2 * 2;

pub type DmaBuffer = [u32; DMA_BUFFER_SIZE];

const START_OF_DRAM2: u32 = 0x30000000;
const DMA_MEM_SIZE: usize = 32 * 1024;

#[link_section = ".sram1_bss"]
#[no_mangle]
static mut TX_BUFFER: DmaBuffer = [0; DMA_BUFFER_SIZE];
#[link_section = ".sram1_bss"]
#[no_mangle]
static mut RX_BUFFER: DmaBuffer = [0; DMA_BUFFER_SIZE];

const FBIPMAX: f32 = 0.999985;
const FBIPMIN: f32 = -FBIPMAX;
const F32_TO_S24_SCALE: f32 = 8388608.0; // 2 ** 23
const S24_TO_F32_SCALE: f32 = 1.0 / F32_TO_S24_SCALE;
const S24_SIGN: i32 = 0x800000;
/// Largest number of audio blocks for a single DMA operation
pub const MAX_TRANSFER_SIZE: usize = BLOCK_SIZE_MAX * 2;

pub type AudioBuffer = [(f32, f32); BLOCK_SIZE_MAX];

type DmaInputStream = dma::Transfer<
    dma::dma::Stream1<stm32::DMA1>,
    stm32::SAI1,
    dma::PeripheralToMemory,
    &'static mut [u32; DMA_BUFFER_SIZE],
    dma::DBTransfer,
>;

type DmaOutputStream = dma::Transfer<
    dma::dma::Stream0<stm32::DMA1>,
    stm32::SAI1,
    dma::MemoryToPeripheral,
    &'static mut [u32; DMA_BUFFER_SIZE],
    dma::DBTransfer,
>;

type StereoIteratorHandle = fn(StereoIterator, &mut Output);

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct S24(i32);

impl From<i32> for S24 {
    fn from(x: i32) -> S24 {
        S24(x)
    }
}

impl From<u32> for S24 {
    fn from(x: u32) -> S24 {
        S24(x as i32)
    }
}

impl From<S24> for i32 {
    fn from(x: S24) -> i32 {
        x.0
    }
}

impl From<S24> for u32 {
    fn from(x: S24) -> u32 {
        x.0 as u32
    }
}

impl From<f32> for S24 {
    fn from(x: f32) -> S24 {
        let x = if x <= FBIPMIN {
            FBIPMIN
        } else if x >= FBIPMAX {
            FBIPMAX
        } else {
            x
        };
        S24((x * F32_TO_S24_SCALE) as i32)
    }
}

impl From<S24> for f32 {
    fn from(x: S24) -> f32 {
        ((x.0 ^ S24_SIGN) - S24_SIGN) as f32 * S24_TO_F32_SCALE
    }
}

/// Core struct for handling audio I/O
pub struct Audio {
    sai: sai::Sai<stm32::SAI1, sai::I2S>,
    input: Input,
    output: Output,
    input_stream: DmaInputStream,
    output_stream: DmaOutputStream,
}

impl Audio {
    /// Setup audio handler
    pub fn new(
        dma1_d: stm32::DMA1,
        dma1_p: rec::Dma1,
        sai1_d: stm32::SAI1,
        sai1_p: rec::Sai1,
        i2c2_d: stm32::I2C2,
        i2c2_p: rec::I2c2,

        // SAI pins
        sai_mclk_a: gpioe::PE2<Analog>,
        sai_sd_b: gpioe::PE3<Analog>,
        sai_fs_a: gpioe::PE4<Analog>,
        sai_sck_a: gpioe::PE5<Analog>,
        sai_sd_a: gpioe::PE6<Analog>,

        //I2C pins
        i2c_scl: gpioh::PH4<Analog>,
        i2c_sda: gpiob::PB11<Analog>,

        clocks: &rcc::CoreClocks,
        mpu: &mut cortex_m::peripheral::MPU,
        scb: &mut cortex_m::peripheral::SCB,
    ) -> Self {
        info!("Setup up DMA...");
        crate::mpu::dma_init(mpu, scb, START_OF_DRAM2 as *mut u32, DMA_MEM_SIZE);

        let dma1_streams = dma::dma::StreamsTuple::new(dma1_d, dma1_p);

        // dma1 stream 0
        let rx_buffer: &'static mut [u32; DMA_BUFFER_SIZE] = unsafe { &mut RX_BUFFER };
        let dma_config = dma::dma::DmaConfig::default()
            .priority(dma::config::Priority::High)
            .memory_increment(true)
            .peripheral_increment(false)
            .circular_buffer(true)
            .fifo_enable(false);
        let mut output_stream: dma::Transfer<_, _, dma::MemoryToPeripheral, _, _> =
            dma::Transfer::init(
                dma1_streams.0,
                unsafe { pac::Peripherals::steal().SAI1 },
                rx_buffer,
                None,
                dma_config,
            );

        // dma1 stream 1
        let tx_buffer: &'static mut [u32; DMA_BUFFER_SIZE] = unsafe { &mut TX_BUFFER };
        let dma_config = dma_config
            .transfer_complete_interrupt(true)
            .half_transfer_interrupt(true);
        let mut input_stream: dma::Transfer<_, _, dma::PeripheralToMemory, _, _> =
            dma::Transfer::init(
                dma1_streams.1,
                unsafe { pac::Peripherals::steal().SAI1 },
                tx_buffer,
                None,
                dma_config,
            );

        info!("Setup up SAI...");
        let sai1_rec = sai1_p.kernel_clk_mux(SAI1SEL_A::PLL3_P);
        let master_config = I2SChanConfig::new(I2SDir::Rx).set_frame_sync_active_high(false);
        let slave_config = I2SChanConfig::new(I2SDir::Tx)
            .set_sync_type(I2SSync::Internal)
            .set_frame_sync_active_high(false);

        let pins_a = (
            sai_mclk_a.into_alternate_af6(),
            sai_sck_a.into_alternate_af6(),
            sai_fs_a.into_alternate_af6(),
            sai_sd_a.into_alternate_af6(),
            Some(sai_sd_b.into_alternate_af6()),
        );

        // Hand off to audio module
        let mut sai = sai1_d.i2s_ch_a(
            pins_a,
            crate::AUDIO_SAMPLE_HZ,
            I2SDataSize::BITS_24,
            sai1_rec,
            clocks,
            I2sUsers::new(master_config).add_slave(slave_config),
        );

        // Manually configure Channel B as transmit stream
        let dma1_reg = unsafe { pac::Peripherals::steal().DMA1 };
        dma1_reg.st[0]
            .cr
            .modify(|_, w| w.dir().peripheral_to_memory());

        // Manually configure Channel A as receive stream
        dma1_reg.st[1]
            .cr
            .modify(|_, w| w.dir().memory_to_peripheral());

        info!("Setup up WM8731 Audio Codec...");
        let i2c2_pins = (i2c_scl.into_alternate_af4(), i2c_sda.into_alternate_af4());

        let mut i2c = i2c2_d.i2c(i2c2_pins, time::Hertz(100_000), i2c2_p, clocks);

        let codec_i2c_address: u8 = 0x1a; // or 0x1b if CSB is high

        // Go through configuration setup
        for (register, value) in REGISTER_CONFIG {
            let register: u8 = (*register).into();
            let value: u8 = (*value).into();
            let byte1: u8 = ((register << 1) & 0b1111_1110) | ((value >> 7) & 0b0000_0001u8);
            let byte2: u8 = value & 0b1111_1111;
            let bytes = [byte1, byte2];

            i2c.write(codec_i2c_address, &bytes).unwrap_or_default();

            // wait ~10us
            asm::delay(5_000);
        }

        info!("Start audio stream...");
        input_stream.start(|_sai1_rb| {
            sai.enable_dma(SaiChannel::ChannelA);
        });

        output_stream.start(|sai1_rb| {
            sai.enable_dma(SaiChannel::ChannelB);

            // wait until sai1's fifo starts to receive data
            info!("Sai1 fifo waiting to receive data.");
            while sai1_rb.chb.sr.read().flvl().is_empty() {}
            info!("Audio started!");
            sai.enable();
            sai.try_send(0, 0).unwrap();
        });
        let input = Input::new(unsafe { &mut RX_BUFFER });
        let output = Output::new(unsafe { &mut TX_BUFFER });
        info!(
            "{:?}, {:?}",
            &input.buffer[0] as *const u32, &output.buffer[0] as *const u32
        );
        Audio {
            sai,
            input_stream,
            output_stream,
            input,
            output,
        }
    }

    /// Check interrupts and set indexes for I/O
    fn read(&mut self) -> bool {
        // Check interrupt(s)
        if self.input_stream.get_half_transfer_flag() {
            self.input_stream.clear_half_transfer_interrupt();
            self.input.set_index(0);
            self.output.set_index(0);
            true
        } else if self.input_stream.get_transfer_complete_flag() {
            self.input_stream.clear_transfer_complete_interrupt();
            self.input.set_index(MAX_TRANSFER_SIZE);
            self.output.set_index(MAX_TRANSFER_SIZE);
            true
        } else {
            false
        }
    }

    /// Directly pass received audio to output without any processing.
    pub fn passthru(&mut self) {
        // Copy data
        if self.read() {
            let mut index = 0;
            let mut out_index = self.output.index;
            while index < MAX_TRANSFER_SIZE {
                self.output.buffer[out_index] = self.input.buffer[index + self.input.index];
                self.output.buffer[out_index + 1] = self.input.buffer[index + self.input.index + 1];
                index += 2;
                out_index += 2;
            }
        }
    }

    /// Gets the audio input from the DMA memory and writes it to buffer
    pub fn get_stereo(&mut self, buffer: &mut AudioBuffer) -> bool {
        if self.read() {
            for (i, (left, right)) in StereoIterator::new(
                &self.input.buffer[self.input.index..self.input.index + MAX_TRANSFER_SIZE],
            )
            .enumerate()
            {
                buffer[i] = (left, right);
            }
            true
        } else {
            false
        }
    }

    fn get_stereo_iter(&mut self) -> Option<StereoIterator> {
        if self.read() {
            return Some(StereoIterator::new(
                &self.input.buffer[self.input.index..MAX_TRANSFER_SIZE],
            ));
        }
        None
    }

    /// Push data to the DMA buffer for output
    /// Call this once per sample per call to [get_stereo()](Audio#get_stereo)
    pub fn push_stereo(&mut self, data: (f32, f32)) -> Result<(), ()> {
        self.output.push(data)
    }
}

struct Input {
    index: usize,
    buffer: &'static DmaBuffer,
}

impl Input {
    /// Create a new Input from a DmaBuffer
    fn new(buffer: &'static DmaBuffer) -> Self {
        Self { index: 0, buffer }
    }

    fn set_index(&mut self, index: usize) {
        self.index = index;
    }

    /// Get StereoIterator(interleaved) iterator
    pub fn get_stereo_iter(&self) -> Option<StereoIterator> {
        Some(StereoIterator::new(&self.buffer[..2]))
    }
}

struct Output {
    index: usize,
    buffer: &'static mut DmaBuffer,
}

impl Output {
    /// Create a new Input from a DmaBuffer
    fn new(buffer: &'static mut DmaBuffer) -> Self {
        Self { index: 0, buffer }
    }

    fn set_index(&mut self, index: usize) {
        self.index = index;
    }

    pub fn push(&mut self, data: (f32, f32)) -> Result<(), ()> {
        if self.index < (MAX_TRANSFER_SIZE * 2) {
            self.buffer[self.index] = S24::from(data.0).into();
            self.buffer[self.index + 1] = S24::from(data.1).into();
            self.index += 2;
            return Ok(());
        }
        Err(())
    }
}

struct StereoIterator<'a> {
    index: usize,
    buf: &'a [u32],
}

impl<'a> StereoIterator<'a> {
    fn new(buf: &'a [u32]) -> Self {
        Self { index: 0, buf }
    }
}

impl Iterator for StereoIterator<'_> {
    type Item = (f32, f32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.buf.len() {
            self.index += 2;
            Some((
                S24(self.buf[self.index - 2] as i32).into(),
                S24(self.buf[self.index - 1] as i32).into(),
            ))
        } else {
            None
        }
    }
}

struct Mono<'a> {
    index: usize,
    buf: &'a [i32],
}

impl<'a> Mono<'a> {
    fn new(buf: &'a [i32]) -> Self {
        Self { index: 0, buf }
    }
}

impl Iterator for Mono<'_> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.buf.len() {
            self.index += 2;
            Some(S24(self.buf[self.index - 1]).into())
        } else {
            None
        }
    }
}

// - WM8731 codec register addresses -------------------------------------------------

#[allow(non_camel_case_types)]
#[derive(Debug, Copy, Clone, IntoPrimitive)]
#[repr(u8)]
enum Register {
    LINVOL = 0x00,
    RINVOL = 0x01,
    LOUT1V = 0x02,
    ROUT1V = 0x03,
    APANA = 0x04,
    APDIGI = 0x05, // 0000_0101
    PWR = 0x06,
    IFACE = 0x07,  // 0000_0111
    SRATE = 0x08,  // 0000_1000
    ACTIVE = 0x09, // 0000_1001
    RESET = 0x0F,
}

const REGISTER_CONFIG: &[(Register, u8)] = &[
    // reset Codec
    (Register::RESET, 0x00),
    // set line inputs 0dB
    (Register::LINVOL, 0x17),
    (Register::RINVOL, 0x17),
    // set headphone to mute
    (Register::LOUT1V, 0x00),
    (Register::ROUT1V, 0x00),
    // set analog and digital routing
    (Register::APANA, 0x12),
    (Register::APDIGI, 0x00),
    // configure power management
    (Register::PWR, 0x42),
    // configure digital format
    (Register::IFACE, 0b1001),
    // set samplerate
    (Register::SRATE, 0x00),
    (Register::ACTIVE, 0x00),
    (Register::ACTIVE, 0x01),
];
