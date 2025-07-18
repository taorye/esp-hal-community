//! This adapter allows for the use of an RMT output channel to easily interact
//! with RGB LEDs and use the convenience functions of the
//! [`smart-leds`](https://crates.io/crates/smart-leds) crate.
//!
//! This is a simple implementation where every LED is adressed in an
//! individual RMT operation. This is working perfectly fine in blocking mode,
//! but in case this is used in combination with interrupts that might disturb
//! the sequential sending, an alternative implementation (addressing the LEDs
//! in a sequence in a single RMT send operation) might be required!
//!
//! ## Example
//!
//! ```rust
//! #![no_std]
//! #![no_main]
//!
//! use esp_backtrace as _;
//! use esp_hal::{rmt::Rmt, time::Rate, Config};
//! use esp_hal_smartled::{smartLedBuffer, SmartLedsAdapter};
//! use smart_leds::{brightness, colors::RED, SmartLedsWrite as _};
//!
//! #[esp_hal::main]
//! fn main() -> ! {
//!     let p = esp_hal::init(Config::default());
//!     let mut led = {
//!         let frequency = Rate::from_mhz(80);
//!         let rmt = Rmt::new(p.RMT, frequency).expect("Failed to initialize RMT0");
//!         SmartLedsAdapter::new(rmt.channel0, p.GPIO2, smartLedBuffer!(1))
//!     };
//!     let level = 10;
//!     led.write(brightness([RED].into_iter(), level)).unwrap();
//!     loop {} // loop forever
//! }
//! ```
//!
//! ## Feature Flags
#![doc = document_features::document_features!()]
#![doc(html_logo_url = "https://avatars.githubusercontent.com/u/46717278")]
#![deny(missing_docs)]
#![no_std]

use core::{fmt::Debug, slice::IterMut};

use esp_hal::{
    clock::Clocks,
    gpio::{interconnect::PeripheralOutput, Level},
    rmt::{
        Error as RmtError, PulseCode, TxChannel, TxChannelAsync, TxChannelConfig, TxChannelCreator,
        TxChannelCreatorAsync,
    },
};
use smart_leds_trait::{SmartLedsWrite, SmartLedsWriteAsync, RGB8};

// Required RMT RAM to drive one LED.
// number of channels (r,g,b -> 3) * pulses per channel 8)
const RMT_RAM_ONE_LED: usize = 3 * 8;

const SK68XX_CODE_PERIOD: u32 = 1250; // 800kHz
const SK68XX_T0H_NS: u32 = 400; // 300ns per SK6812 datasheet, 400 per WS2812. Some require >350ns for T0H. Others <500ns for T0H.
const SK68XX_T0L_NS: u32 = SK68XX_CODE_PERIOD - SK68XX_T0H_NS;
const SK68XX_T1H_NS: u32 = 850; // 900ns per SK6812 datasheet, 850 per WS2812. > 550ns is sometimes enough. Some require T1H >= 2 * T0H. Some require > 300ns T1L.
const SK68XX_T1L_NS: u32 = SK68XX_CODE_PERIOD - SK68XX_T1H_NS;

/// All types of errors that can happen during the conversion and transmission
/// of LED commands
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum LedAdapterError {
    /// Raised in the event that the provided data container is not large enough
    BufferSizeExceeded,
    /// Raised if something goes wrong in the transmission,
    TransmissionError(RmtError),
}

impl From<RmtError> for LedAdapterError {
    fn from(e: RmtError) -> Self {
        LedAdapterError::TransmissionError(e)
    }
}

fn led_pulses_for_clock(src_clock: u32) -> (u32, u32) {
    (
        PulseCode::new(
            Level::High,
            ((SK68XX_T0H_NS * src_clock) / 1000) as u16,
            Level::Low,
            ((SK68XX_T0L_NS * src_clock) / 1000) as u16,
        ),
        PulseCode::new(
            Level::High,
            ((SK68XX_T1H_NS * src_clock) / 1000) as u16,
            Level::Low,
            ((SK68XX_T1L_NS * src_clock) / 1000) as u16,
        ),
    )
}

fn led_config() -> TxChannelConfig {
    TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_carrier_modulation(false)
        .with_idle_output(true)
}

fn convert_rgb_to_pulses(
    value: RGB8,
    mut_iter: &mut IterMut<u32>,
    pulses: (u32, u32),
) -> Result<(), LedAdapterError> {
    convert_rgb_channel_to_pulses(value.g, mut_iter, pulses)?;
    convert_rgb_channel_to_pulses(value.r, mut_iter, pulses)?;
    convert_rgb_channel_to_pulses(value.b, mut_iter, pulses)?;
    Ok(())
}

fn convert_rgb_channel_to_pulses(
    channel_value: u8,
    mut_iter: &mut IterMut<u32>,
    pulses: (u32, u32),
) -> Result<(), LedAdapterError> {
    for position in [128, 64, 32, 16, 8, 4, 2, 1] {
        *mut_iter.next().ok_or(LedAdapterError::BufferSizeExceeded)? =
            match channel_value & position {
                0 => pulses.0,
                _ => pulses.1,
            }
    }

    Ok(())
}

/// Function to calculate the required RMT buffer size for a given number of LEDs when using
/// the blocking API.
///
/// This buffer size is calculated for the synchronous API provided by the [SmartLedsAdapter].
/// [buffer_size_async] should be used for the asynchronous API.
pub const fn buffer_size(num_leds: usize) -> usize {
    // 1 additional pulse for the end delimiter
    num_leds * RMT_RAM_ONE_LED + 1
}

/// Macro to allocate a buffer sized for a specific number of LEDs to be
/// addressed.
///
/// Attempting to use more LEDs that the buffer is configured for will result in
/// an `LedAdapterError:BufferSizeExceeded` error.
#[macro_export]
macro_rules! smart_led_buffer {
    ( $num_leds: expr ) => {
        [0u32; $crate::buffer_size($num_leds)]
    };
}

/// Deprecated alias for [smart_led_buffer] macro.
#[macro_export]
#[deprecated]
macro_rules! smartLedBuffer {
    ( $num_leds: expr ) => {
        smart_led_buffer!($num_leds);
    };
}

/// Adapter taking an RMT channel and a specific pin and providing RGB LED
/// interaction functionality using the `smart-leds` crate
pub struct SmartLedsAdapter<TX, const BUFFER_SIZE: usize>
where
    TX: TxChannel,
{
    channel: Option<TX>,
    rmt_buffer: [u32; BUFFER_SIZE],
    pulses: (u32, u32),
}

impl<'d, TX, const BUFFER_SIZE: usize> SmartLedsAdapter<TX, BUFFER_SIZE>
where
    TX: TxChannel,
{
    /// Create a new adapter object that drives the pin using the RMT channel.
    pub fn new<C, O>(
        channel: C,
        pin: O,
        rmt_buffer: [u32; BUFFER_SIZE],
    ) -> SmartLedsAdapter<TX, BUFFER_SIZE>
    where
        O: PeripheralOutput<'d>,
        C: TxChannelCreator<'d, TX>,
    {
        let channel = channel.configure(pin, led_config()).unwrap();

        // Assume the RMT peripheral is set up to use the APB clock
        let src_clock = Clocks::get().apb_clock.as_mhz();

        Self {
            channel: Some(channel),
            rmt_buffer,
            pulses: led_pulses_for_clock(src_clock),
        }
    }
}

impl<TX, const BUFFER_SIZE: usize> SmartLedsWrite for SmartLedsAdapter<TX, BUFFER_SIZE>
where
    TX: TxChannel,
{
    type Error = LedAdapterError;
    type Color = RGB8;

    /// Convert all RGB8 items of the iterator to the RMT format and
    /// add them to internal buffer, then start a singular RMT operation
    /// based on that buffer.
    fn write<T, I>(&mut self, iterator: T) -> Result<(), Self::Error>
    where
        T: IntoIterator<Item = I>,
        I: Into<Self::Color>,
    {
        // We always start from the beginning of the buffer
        let mut seq_iter = self.rmt_buffer.iter_mut();

        // Add all converted iterator items to the buffer.
        // This will result in an `BufferSizeExceeded` error in case
        // the iterator provides more elements than the buffer can take.
        for item in iterator {
            convert_rgb_to_pulses(item.into(), &mut seq_iter, self.pulses)?;
        }

        // Finally, add an end element.
        *seq_iter.next().ok_or(LedAdapterError::BufferSizeExceeded)? = 0;

        // Perform the actual RMT operation. We use the u32 values here right away.
        let channel = self.channel.take().unwrap();
        match channel.transmit(&self.rmt_buffer)?.wait() {
            Ok(chan) => {
                self.channel = Some(chan);
                Ok(())
            }
            Err((e, chan)) => {
                self.channel = Some(chan);
                Err(LedAdapterError::TransmissionError(e))
            }
        }
    }
}

/// Support for asynchronous and non-blocking use of the RMT peripheral to drive smart LEDs.
///
/// Function to calculate the required RMT buffer size for a given number of LEDs when using
/// the asynchronous API. This buffer size is calculated for the asynchronous API provided by the
/// [SmartLedsAdapterAsync]. [buffer_size] should be used for the synchronous API.
pub const fn buffer_size_async(num_leds: usize) -> usize {
    // 1 byte end delimiter for each transfer.
    num_leds * (RMT_RAM_ONE_LED + 1)
}

/// Adapter taking an RMT channel and a specific pin and providing RGB LED
/// interaction functionality.
pub struct SmartLedsAdapterAsync<Tx, const BUFFER_SIZE: usize> {
    channel: Tx,
    rmt_buffer: [u32; BUFFER_SIZE],
    pulses: (u32, u32),
}

impl<'d, Tx: TxChannelAsync, const BUFFER_SIZE: usize> SmartLedsAdapterAsync<Tx, BUFFER_SIZE> {
    /// Create a new adapter object that drives the pin using the RMT channel.
    pub fn new<C, O>(
        channel: C,
        pin: O,
        rmt_buffer: [u32; BUFFER_SIZE],
    ) -> SmartLedsAdapterAsync<Tx, BUFFER_SIZE>
    where
        O: PeripheralOutput<'d>,
        C: TxChannelCreatorAsync<'d, Tx>,
    {
        let channel = channel.configure(pin, led_config()).unwrap();

        // Assume the RMT peripheral is set up to use the APB clock
        let src_clock = Clocks::get().apb_clock.as_mhz();

        Self {
            channel,
            rmt_buffer,
            pulses: led_pulses_for_clock(src_clock),
        }
    }

    fn prepare_rmt_buffer<I: Into<RGB8>>(
        &mut self,
        iterator: impl IntoIterator<Item = I>,
    ) -> Result<(), LedAdapterError> {
        // We always start from the beginning of the buffer
        let mut seq_iter = self.rmt_buffer.iter_mut();

        // Add all converted iterator items to the buffer.
        // This will result in an `BufferSizeExceeded` error in case
        // the iterator provides more elements than the buffer can take.
        for item in iterator {
            Self::convert_rgb_to_pulse(item.into(), &mut seq_iter, self.pulses)?;
        }
        Ok(())
    }

    /// Converts a RGB value to the correspodnign pulse value.
    fn convert_rgb_to_pulse(
        value: RGB8,
        mut_iter: &mut IterMut<u32>,
        pulses: (u32, u32),
    ) -> Result<(), LedAdapterError> {
        convert_rgb_to_pulses(value, mut_iter, pulses)?;
        *mut_iter.next().ok_or(LedAdapterError::BufferSizeExceeded)? = 0;

        Ok(())
    }
}

impl<Tx: TxChannelAsync, const BUFFER_SIZE: usize> SmartLedsWriteAsync
    for SmartLedsAdapterAsync<Tx, BUFFER_SIZE>
{
    type Error = LedAdapterError;
    type Color = RGB8;

    /// Convert all RGB8 items of the iterator to the RMT format and
    /// add them to internal buffer, then start perform all asynchronous operations based on
    /// that buffer.
    async fn write<T, I>(&mut self, iterator: T) -> Result<(), Self::Error>
    where
        T: IntoIterator<Item = I>,
        I: Into<Self::Color>,
    {
        self.prepare_rmt_buffer(iterator)?;
        for chunk in self.rmt_buffer.chunks(RMT_RAM_ONE_LED + 1) {
            self.channel
                .transmit(chunk)
                .await
                .map_err(LedAdapterError::TransmissionError)?;
        }
        Ok(())
    }
}
