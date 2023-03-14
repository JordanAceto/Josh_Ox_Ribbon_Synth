use stm32l4xx_hal::{
    adc::{SampleTime, Sequence, ADC},
    delay::Delay,
    device::SPI1,
    gpio::{Alternate, Input, Output, Pin, PullUp, PushPull, H8, L8},
    hal::spi::{Mode, Phase, Polarity},
    pac::{interrupt, ADC1, DMA1, TIM2, TIM6, USART1},
    prelude::*,
    serial,
    spi::Spi,
    timer::Timer,
};

use nb::block;

/// The physical board structure is represented here
pub struct Board {
    // USART for MIDI
    midi_tx: serial::Tx<USART1>,
    midi_rx: serial::Rx<USART1>,

    // SPI for DAC
    spi: Spi<
        SPI1,
        (
            Pin<Alternate<PushPull, 5>, L8, 'B', 3>, // SCK
            Pin<Alternate<PushPull, 5>, L8, 'B', 4>, // SDI
            Pin<Alternate<PushPull, 5>, L8, 'B', 5>, // SDO
        ),
    >,
    nss: Pin<Output<PushPull>, H8, 'A', 15>, // manual chip select

    // general purpose delay
    delay: Delay,

    // 2 pins for the 3-position QUANTIZE MODE switch
    mode_switch: (
        Pin<Input<PullUp>, L8, 'B', 6>,
        Pin<Input<PullUp>, L8, 'B', 7>,
    ),

    // ribbon gate output
    gate_pin: Pin<Output<PushPull>, L8, 'A', 5>,
}

impl Board {
    /// `Board::init()` is the board structure with all peripherals initialized.
    pub fn init() -> Self {
        ////////////////////////////////////////////////////////////////////////
        //
        // general peripheral housekeeping, core peripherals and clocks
        //
        ////////////////////////////////////////////////////////////////////////
        let cp = cortex_m::Peripherals::take().unwrap();
        let dp = stm32l4xx_hal::pac::Peripherals::take().unwrap();
        let mut flash = dp.FLASH.constrain();
        let mut rcc = dp.RCC.constrain();
        let mut pwr = dp.PWR.constrain(&mut rcc.apb1r1);

        let clocks = rcc
            .cfgr
            .sysclk(SYST_CLK_FREQ_MHZ.MHz())
            .pclk1(SYST_CLK_FREQ_MHZ.MHz())
            .pclk2(SYST_CLK_FREQ_MHZ.MHz())
            .freeze(&mut flash.acr, &mut pwr);

        let mut gpioa = dp.GPIOA.split(&mut rcc.ahb2);
        let mut gpiob = dp.GPIOB.split(&mut rcc.ahb2);

        let dma_channels = dp.DMA1.split(&mut rcc.ahb1);

        let mut delay = Delay::new(cp.SYST, clocks);

        ////////////////////////////////////////////////////////////////////////
        //
        // ADC
        //
        ////////////////////////////////////////////////////////////////////////

        // gpio pins which are configured as analog inputs on the physical PCB
        let mut adc_pins = (
            gpioa.pa0.into_analog(&mut gpioa.moder, &mut gpioa.pupdr),
            gpioa.pa1.into_analog(&mut gpioa.moder, &mut gpioa.pupdr),
            gpioa.pa2.into_analog(&mut gpioa.moder, &mut gpioa.pupdr),
            gpioa.pa3.into_analog(&mut gpioa.moder, &mut gpioa.pupdr),
            gpioa.pa4.into_analog(&mut gpioa.moder, &mut gpioa.pupdr),
        );

        // configure DMA1 to transfer ADC readings to the buffer
        let mut dma1_ch1 = dma_channels.1;
        unsafe {
            dma1_ch1.set_peripheral_address(&dp.ADC1.dr as *const _ as u32, false);
            dma1_ch1.set_memory_address(ADC_DMA_BUFF.as_ptr() as u32, true);
        }
        dma1_ch1.set_transfer_length(NUM_ADC_DMA_SIGNALS as u16);
        unsafe {
            (*DMA1::ptr()).ccr1.modify(|_, w| {
                w.msize()
                    .bits16()
                    .psize()
                    .bits16()
                    .minc()
                    .enabled()
                    .circ()
                    .enabled()
                    .en()
                    .set_bit()
            });
        }

        // configure the ADC
        let mut adc1 = ADC::new(
            dp.ADC1,
            dp.ADC_COMMON,
            &mut rcc.ahb2,
            &mut rcc.ccipr,
            &mut delay,
        );
        adc1.configure_sequence(&mut adc_pins.0, Sequence::One, SampleTime::Cycles640_5);
        adc1.configure_sequence(&mut adc_pins.1, Sequence::Two, SampleTime::Cycles640_5);
        adc1.configure_sequence(&mut adc_pins.2, Sequence::Three, SampleTime::Cycles640_5);
        adc1.configure_sequence(&mut adc_pins.3, Sequence::Four, SampleTime::Cycles640_5);
        adc1.configure_sequence(&mut adc_pins.4, Sequence::Five, SampleTime::Cycles640_5);
        unsafe {
            // configure hardware oversampler for 16 bit resolution
            (*ADC1::ptr()).cfgr2.modify(|_, w| {
                w.ovss()
                    .bits(0b0001) // shift right by 1
                    .ovsr()
                    .bits(0b100) // oversample 32x
                    .rovse()
                    .set_bit()
            });
            // enable continuous DMA mode
            (*ADC1::ptr())
                .cfgr
                .modify(|_, w| w.dmacfg().set_bit().dmaen().set_bit().cont().set_bit());
        }

        dma1_ch1.start();
        adc1.start_conversion();

        ////////////////////////////////////////////////////////////////////////
        //
        // TIMx periodic timers
        //
        ////////////////////////////////////////////////////////////////////////
        let _tim2 = Timer::tim2(dp.TIM2, TIM2_FREQ_HZ.Hz(), clocks, &mut rcc.apb1r1);

        let _tim6 = Timer::tim6(dp.TIM6, TIM6_FREQ_HZ.Hz(), clocks, &mut rcc.apb1r1);

        ////////////////////////////////////////////////////////////////////////
        //
        // USART
        //
        ////////////////////////////////////////////////////////////////////////
        let tx_pin = gpioa
            .pa9
            .into_alternate(&mut gpioa.moder, &mut gpioa.otyper, &mut gpioa.afrh);
        let rx_pin =
            gpioa
                .pa10
                .into_alternate(&mut gpioa.moder, &mut gpioa.otyper, &mut gpioa.afrh);

        let usart = serial::Serial::usart1(
            dp.USART1,
            (tx_pin, rx_pin),
            serial::Config::default().baudrate(MIDI_BAUD_RATE_HZ.bps()),
            clocks,
            &mut rcc.apb2,
        );
        let (tx, rx) = usart.split();

        ////////////////////////////////////////////////////////////////////////
        //
        // SPI
        //
        ////////////////////////////////////////////////////////////////////////
        let sck = gpiob
            .pb3
            .into_alternate(&mut gpiob.moder, &mut gpiob.otyper, &mut gpiob.afrl);
        let sdi = gpiob
            .pb4
            .into_alternate(&mut gpiob.moder, &mut gpiob.otyper, &mut gpiob.afrl);
        let sdo = gpiob
            .pb5
            .into_alternate(&mut gpiob.moder, &mut gpiob.otyper, &mut gpiob.afrl);

        let nss = gpioa.pa15.into_push_pull_output_in_state(
            &mut gpioa.moder,
            &mut gpioa.otyper,
            PinState::High,
        );

        let spi = Spi::spi1(
            dp.SPI1,
            (sck, sdi, sdo),
            Mode {
                phase: Phase::CaptureOnFirstTransition,
                polarity: Polarity::IdleHigh,
            },
            SPI_CLK_FREQ_MHZ.MHz(),
            clocks,
            &mut rcc.apb2,
        );

        ////////////////////////////////////////////////////////////////////////
        //
        // 3-way Mode switch
        //
        ////////////////////////////////////////////////////////////////////////
        let mode_switch = (
            gpiob
                .pb6
                .into_pull_up_input(&mut gpiob.moder, &mut gpiob.pupdr),
            gpiob
                .pb7
                .into_pull_up_input(&mut gpiob.moder, &mut gpiob.pupdr),
        );

        ////////////////////////////////////////////////////////////////////////
        //
        // Gate pin
        //
        ////////////////////////////////////////////////////////////////////////
        let gate_pin = gpioa
            .pa5
            .into_push_pull_output(&mut gpioa.moder, &mut gpioa.otyper);

        Self {
            midi_tx: tx,
            midi_rx: rx,
            spi,
            nss,
            delay,
            mode_switch,
            gate_pin,
        }
    }

    /// `board.read_adc(p)` is the digitized analog value on pin `p` in the range `[0.0, +1.0]`
    pub fn read_adc(&mut self, pin: AdcPin) -> f32 {
        // the values are already stored in the buffer via DMA
        unsafe { adc_fs_to_normalized_fl(ADC_DMA_BUFF[pin as usize]) }
    }

    /// `board.dac8164_write(v, c)` writes the normalized value `v` in the range `[0.0, +1.0]` to channel `c` of the onboard DAC.
    pub fn dac8164_write(&mut self, val: f32, channel: Dac8164Channel) {
        let val_u14 = normalized_fl_to_dac_fs(val);

        // move the value out of DB0 and DB1
        let val_u14 = val_u14 << 2;
        // split it into bytes
        let low_byte = (val_u14 & 0xFF) as u8;
        let mid_byte = (val_u14 >> 8) as u8;
        let high_byte = channel as u8 | (1 << 4); // set LDO for immediate update

        self.spi_write(&[high_byte, mid_byte, low_byte]);
    }

    /// `board.read_mode_switch()` is the enumerated state of the 3-way mode switch.
    pub fn read_mode_switch(&self) -> Switch3wayState {
        // The physical switch on the PCB is a SPDT on-off-on switch which grounds
        // either PB6, PB7, or neither pins depending on the position.
        match (self.mode_switch.0.is_low(), self.mode_switch.1.is_low()) {
            (false, true) => Switch3wayState::Up,
            (false, false) => Switch3wayState::Middle,
            _ => Switch3wayState::Down, // should only happen with (true, false) but catch unlikely (true, true) as well
                                        // (true, true) means that something is wrong with the switch, but the show must go on
        }
    }

    /// `board.serial_write(b)` writes the byte `b` via the USART in blocking fashion.
    pub fn serial_write(&mut self, byte: u8) {
        block!(self.midi_tx.write(byte)).ok();
    }

    /// `board.serial_read()` is the optional byte read from the USART.
    pub fn serial_read(&mut self) -> Option<u8> {
        match self.midi_rx.read() {
            Ok(byte) => Some(byte),
            _ => None,
        }
    }

    /// `board.spi_write(words)` writes the words via SPI.
    fn spi_write(&mut self, words: &[u8]) {
        self.nss.set_low();
        self.spi.write(words).unwrap();
        self.nss.set_high();
    }

    /// `board.set_gate(val)` sets the state of the gate pin to `val`.
    pub fn set_gate(&mut self, val: bool) {
        self.gate_pin.set_state(PinState::from(val));
    }

    /// `board.delay_ms(ms)` causes the board to busy-wait for `ms` milliseconds
    pub fn delay_ms(&mut self, ms: u32) {
        self.delay.delay_ms(ms);
    }

    /// `board.delay_us(us)` causes the board to busy-wait for `us` microseconds
    pub fn delay_us(&mut self, us: u32) {
        self.delay.delay_us(us);
    }

    /// board.get_tim2_timeout()` is true iff timer TIM2 has timed out, self clearing.
    pub fn get_tim2_timeout(&self) -> bool {
        unsafe {
            if (*TIM2::ptr()).sr.read().uif().bit() {
                (*TIM2::ptr()).sr.modify(|_, w| w.uif().clear());
                true
            } else {
                false
            }
        }
    }

    /// board.get_tim6_timeout()` is true iff timer TIM6 has timed out, self clearing.
    pub fn get_tim6_timeout(&self) -> bool {
        unsafe {
            if (*TIM6::ptr()).sr.read().uif().bit() {
                (*TIM6::ptr()).sr.modify(|_, w| w.uif().clear());
                true
            } else {
                false
            }
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
//
// Public constants
//
////////////////////////////////////////////////////////////////////////////////

/// The frequenct of the main system clock
pub const SYST_CLK_FREQ_MHZ: u32 = 80;

/// The frequency for periodic timer TIM2
pub const TIM2_FREQ_HZ: u32 = 5_000;

/// The frequency for periodic timer TIM6
pub const TIM6_FREQ_HZ: u32 = 30;

/// The SPI clock frequency to use
const SPI_CLK_FREQ_MHZ: u32 = 20;

/// The maximum value that can be produced by the Analog to Digital Converters.
pub const ADC_MAX: u16 = 0xFFF0;

/// The maximum value that can be written to the onboard Digital to Analog Converter.
pub const DAC_MAX: u16 = (1 << 14) - 1;

/// The baud rate required for MIDI communication
pub const MIDI_BAUD_RATE_HZ: u32 = 31_250;

////////////////////////////////////////////////////////////////////////////////
//
// Private constants and static variables
//
////////////////////////////////////////////////////////////////////////////////

/// ADC readings are stored in a static array via DMA
const NUM_ADC_DMA_SIGNALS: usize = 5;
static mut ADC_DMA_BUFF: [u16; NUM_ADC_DMA_SIGNALS] = [0; NUM_ADC_DMA_SIGNALS];

////////////////////////////////////////////////////////////////////////////////
//
// Private helper functions
//
////////////////////////////////////////////////////////////////////////////////

/// `adc_fs_to_normalized_fl(v)` is the integer adc value with the full scale normalized to [0.0, +1.0]
///
/// If the input value would overflow the output range it is clamped.
fn adc_fs_to_normalized_fl(val: u16) -> f32 {
    let val = val.min(ADC_MAX); // don't need to clamp negative values, it's already unsigned

    (val as f32) / (ADC_MAX as f32)
}

/// `normalized_fl_to_dac_fs(v)` is the normalized [0.0, +1.0] value expanded to DAC full scale range.
///
/// If the input value would overflow the output range it is clamped.
fn normalized_fl_to_dac_fs(val: f32) -> u16 {
    let val = val.min(1.0_f32).max(0.0_f32);

    (val * DAC_MAX as f32) as u16
}

#[interrupt]
fn TIM2() {}

////////////////////////////////////////////////////////////////////////////////
//
// Public enums
//
////////////////////////////////////////////////////////////////////////////////

/// Pins which may be read by the ADC are represented here
#[derive(Clone, Copy)]
pub enum AdcPin {
    PA0 = 0,
    PA1 = 1,
    PA2 = 2,
    PA3 = 3,
    PA4 = 4,
}

/// Channels of the onboard DAC are represented here
#[derive(Clone, Copy)]
pub enum Dac8164Channel {
    A = 0b000,
    B = 0b010,
    C = 0b100,
    D = 0b110,
}

/// Valid states of a 3-way switch are represented here
#[derive(Clone, Copy)]
pub enum Switch3wayState {
    Up,
    Middle,
    Down,
}
