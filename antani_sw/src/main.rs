#![no_std]
#![no_main]

use core::f64;

use defmt::*;
use defmt_rtt as _;
use embassy_executor::{InterruptExecutor, Spawner};
use embassy_rp::adc;
use embassy_rp::gpio::Input;
use embassy_rp::gpio::Pull;
use embassy_rp::interrupt;
use embassy_rp::interrupt::{InterruptExt, Priority};

use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use embassy_sync::channel::{Channel, Sender};
use embassy_sync::lazy_lock::LazyLock;
use embassy_time::with_timeout;
use embassy_time::Instant;
use embassy_time::{Duration, Ticker, Timer};

use embassy_rp::bind_interrupts;
use heapless::Vec;
use infrared::{protocol::NecDebug, Receiver};
use panic_probe as _;

mod rgbeffects;
mod ws2812;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

use rand::rngs::SmallRng;
use rand::SeedableRng;
use rgbeffects::AnimationPattern;
use rgbeffects::ColorPalette;
use rgbeffects::FragmentShader;
use rgbeffects::LedPattern;
use rgbeffects::RenderCommand;
use rgbeffects::RenderManager;
use rgbeffects::RunEffect;
use smart_leds::RGB8;
use ws2812::Ws2812;

const LED_MATRIX_WIDTH: usize = 3;
const LED_MATRIX_HEIGHT: usize = 3;
const LED_MATRIX_SIZE: usize = LED_MATRIX_WIDTH * LED_MATRIX_HEIGHT;

static GAMMA_CORRECTION: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 5, 5, 5,
    5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11, 12, 12, 13, 13, 13, 14,
    14, 15, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20, 21, 21, 22, 22, 23, 24, 24, 25, 25, 26, 27,
    27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 35, 35, 36, 37, 38, 39, 39, 40, 41, 42, 43, 44, 45, 46,
    47, 48, 49, 50, 50, 51, 52, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 66, 67, 68, 69, 70, 72,
    73, 74, 75, 77, 78, 79, 81, 82, 83, 85, 86, 87, 89, 90, 92, 93, 95, 96, 98, 99, 101, 102, 104,
    105, 107, 109, 110, 112, 114, 115, 117, 119, 120, 122, 124, 126, 127, 129, 131, 133, 135, 137,
    138, 140, 142, 144, 146, 148, 150, 152, 154, 156, 158, 160, 162, 164, 167, 169, 171, 173, 175,
    177, 180, 182, 184, 186, 189, 191, 193, 196, 198, 200, 203, 205, 208, 210, 213, 215, 218, 220,
    223, 225, 228, 231, 233, 236, 239, 241, 244, 247, 249, 252, 255,
];

struct LedMatrix {
    pub framebuffer: [RGB8; LED_MATRIX_SIZE],
    corrected_gain: f32,
    raw_gain: f32,
}

#[allow(dead_code)]
impl LedMatrix {
    fn new() -> Self {
        Self {
            framebuffer: [(0, 0, 0).into(); LED_MATRIX_SIZE],
            corrected_gain: 0.5,
            raw_gain: 1.0,
        }
    }

    fn set_gain(&mut self, gain: f32) {
        self.corrected_gain = gain;
    }

    fn set_raw_gain(&mut self, gain: f32) {
        self.raw_gain = gain;
    }

    fn set_pixel(&mut self, x: usize, y: usize, rgb: RGB8) {
        if x < LED_MATRIX_WIDTH && y < LED_MATRIX_HEIGHT {
            self.framebuffer[y * LED_MATRIX_WIDTH + x] = rgb;
        }
    }

    fn set_all(&mut self, rgb: RGB8) {
        for i in 0..LED_MATRIX_SIZE {
            self.framebuffer[i] = rgb;
        }
    }

    fn clear(&mut self) {
        self.set_all((0, 0, 0).into());
    }

    fn render(&mut self, pattern: &LedPattern, colour: RGB8) {
        let colour = RGB8 {
            r: (colour.r as f32 * self.corrected_gain) as u8,
            g: (colour.g as f32 * self.corrected_gain) as u8,
            b: (colour.b as f32 * self.corrected_gain) as u8,
        };

        // gamma correction

        let colour = RGB8 {
            r: GAMMA_CORRECTION[colour.r as usize],
            g: GAMMA_CORRECTION[colour.g as usize],
            b: GAMMA_CORRECTION[colour.b as usize],
        };

        let colour = RGB8 {
            r: (colour.r as f32 * self.raw_gain) as u8,
            g: (colour.g as f32 * self.raw_gain) as u8,
            b: (colour.b as f32 * self.raw_gain) as u8,
        };

        // this maps bits in the pattern bitfield to the corresponding led in the matrix
        let bit_offsets = [
            (0, 2), // bit 0, first led
            (0, 1),
            (0, 0),
            (1, 2),
            (1, 1),
            (1, 0),
            (2, 2),
            (2, 1),
            (2, 0), // bit 8, the last led
        ];

        for (i, (x, y)) in bit_offsets.iter().enumerate() {
            if pattern.pattern & (1 << i) != 0 {
                self.set_pixel(*x, *y, colour);
            }
        }
    }
}

type AppSender = Sender<'static, CriticalSectionRawMutex, AppCommand, 8>;
enum AppCommand {
    ThermalThrottleMultiplier(f32), // 1.0 = no throttle, 0.0 = full throttle
    IrCommand(u32),
    ShortButtonPress,
    LongButtonPress,
}
static CHANNEL: Channel<CriticalSectionRawMutex, AppCommand, 8> = Channel::new();

struct Patterns {
    pub power_100: LedPattern,
    pub power_75: LedPattern,
    pub power_50: LedPattern,
    pub power_25: LedPattern,
    pub glider: LedPattern,
    pub all_on: LedPattern,
    pub everything_once: AnimationPattern,
}

static PATTERNS: LazyLock<Patterns> = LazyLock::new(|| Patterns {
    // patterns for light power
    power_100: LedPattern::new(0b111111111),
    power_75: LedPattern::new(0b000111111),
    power_50: LedPattern::new(0b000000111),
    power_25: LedPattern::new(0b000000001),

    glider: LedPattern::new(0b010001111),
    all_on: LedPattern::new(0b111111111),
    everything_once: AnimationPattern::new(&[
        0b100000000,
        0b010000000,
        0b001000000,
        0b000100000,
        0b000010000,
        0b000001000,
        0b000000100,
        0b000000010,
        0b000000001,
    ]),
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Program start");
    let p = embassy_rp::init(Default::default());

    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);

    let adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let ts = adc::Channel::new_temp_sensor(p.ADC_TEMP_SENSOR);
    unwrap!(spawner.spawn(temperature(adc, ts, CHANNEL.sender())));

    // let mut ir_blaster = pins.gpio11.into_push_pull_output();
    let ir_sensor = Input::new(p.PIN_10, Pull::None);
    let user_button = Input::new(p.PIN_9, Pull::Up);

    unwrap!(spawner.spawn(button_driver(user_button, CHANNEL.sender())));

    interrupt::SWI_IRQ_1.set_priority(Priority::P3);
    let highpriority_spawner = EXECUTOR_HIGH.start(interrupt::SWI_IRQ_1);
    unwrap!(highpriority_spawner.spawn(ir_receiver(ir_sensor, CHANNEL.sender())));

    let mut renderman = RenderManager {
        mtrx: LedMatrix::new(),
        rng: SmallRng::seed_from_u64(69420),
    };

    let mut ws2812 = Ws2812::new(&mut common, sm0, p.DMA_CH0, p.PIN_19);

    let patterns = PATTERNS.get();

    println!("Starting loop");

    let scenes: [Vec<RenderCommand, 8>; 3] = [
        // strobing glider
        Vec::from_slice(&[RenderCommand {
            effect: RunEffect::SimplePattern(patterns.glider),
            color: ColorPalette::Solid((0, 0, 255).into()),
            color_shaders: Vec::from_slice(&[
                FragmentShader::Breathing(0.7),
                FragmentShader::Blinking(10.0),
            ])
            .unwrap(),
        }])
        .unwrap(),
        // glider with particles
        Vec::from_slice(&[
            RenderCommand {
                effect: RunEffect::SimplePattern(patterns.glider),
                color: ColorPalette::Solid((0, 0, 255).into()),
                color_shaders: Vec::from_slice(&[FragmentShader::Breathing(0.7)]).unwrap(),
            },
            RenderCommand {
                effect: RunEffect::AnimationPattern(&patterns.everything_once, 6.0),
                color: ColorPalette::Rainbow(0.25, 0.0),
                color_shaders: Vec::new(),
            },
            RenderCommand {
                effect: RunEffect::ReverseAnimationPattern(&patterns.everything_once, 6.0),
                color: ColorPalette::Rainbow(0.25, 0.5),
                color_shaders: Vec::new(),
            },
        ])
        .unwrap(),
        // double rainbow glider
        Vec::from_slice(&[
            RenderCommand {
                effect: RunEffect::SimplePattern(patterns.all_on),
                color: ColorPalette::Rainbow(0.25, 0.0),
                color_shaders: Vec::new(),
            },
            RenderCommand {
                effect: RunEffect::SimplePattern(patterns.glider),
                color: ColorPalette::Rainbow(0.25, 0.5),
                color_shaders: Vec::new(),
            },
        ])
        .unwrap(),
    ];

    let gains = [1.0, 0.7, 0.5, 0.25];
    let mut scene_id = 0;
    let mut gain_id = 0;
    loop {
        //t = timer.get_counter().ticks() as f64 / 1_000_000.0;
        let t = Instant::now().as_micros() as f64 / 1_000_000.0;

        renderman.mtrx.set_gain(gains[gain_id]);

        if let Ok(ch) = CHANNEL.try_receive() {
            match ch {
                AppCommand::ThermalThrottleMultiplier(gain) => {
                    renderman.mtrx.set_raw_gain(gain);
                    println!("Thermal throttle multiplier: {}", gain);
                }
                AppCommand::IrCommand(cmd) => {
                    println!("IR command: {}", cmd);
                }
                AppCommand::ShortButtonPress => {
                    println!("Short button press");
                    scene_id = (scene_id + 1) % scenes.len();
                }
                AppCommand::LongButtonPress => {
                    // todo: deduplicate the rendering code here
                    println!("Long button press");
                    gain_id = (gain_id + 1) % gains.len();

                    renderman.mtrx.set_gain(gains[gain_id]);

                    let patt = match gain_id {
                        0 => &patterns.power_100,
                        1 => &patterns.power_75,
                        2 => &patterns.power_50,
                        3 => &patterns.power_25,
                        _ => &patterns.power_100,
                    };

                    renderman.render(
                        &[RenderCommand {
                            effect: RunEffect::SimplePattern(*patt),
                            color: ColorPalette::Solid((255, 255, 255).into()),
                            color_shaders: Vec::new(),
                        }],
                        t,
                    );

                    ws2812.write(&renderman.mtrx.framebuffer).await;
                    Timer::after_millis(1000).await;
                    renderman.mtrx.clear();
                }
            }
        }

        renderman.render(&scenes[scene_id], t);

        ws2812.write(&renderman.mtrx.framebuffer).await;
        Timer::after_millis(1).await;
        renderman.mtrx.clear();
    }
}

static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();

#[interrupt]
unsafe fn SWI_IRQ_1() {
    EXECUTOR_HIGH.on_interrupt()
}

#[embassy_executor::task]
async fn ir_receiver(ir_sensor: Input<'static>, control: AppSender) {
    let mut int_receiver: Receiver<NecDebug, embassy_rp::gpio::Input> = Receiver::builder()
        .rc5()
        .frequency(1_000_000)
        .pin(ir_sensor)
        .protocol()
        .build();

    loop {
        int_receiver.pin_mut().wait_for_any_edge().await;

        if let Ok(Some(cmd)) = int_receiver.event_instant(Instant::now().as_ticks() as u32) {
            control.send(AppCommand::IrCommand(cmd.bits)).await;
        }
    }
}

#[embassy_executor::task]
async fn temperature(
    mut adc: adc::Adc<'static, adc::Async>,
    mut ts: adc::Channel<'static>,
    control: AppSender,
) {
    let mut ticker = Ticker::every(Duration::from_secs(1));

    loop {
        let temp = adc.read(&mut ts).await.unwrap();

        // TODO: yeah let's waste precious CPU cycles to calculate the temperature before checking if we need to throttle
        let adc_voltage = (3.3 / 4096.0) * temp as f64;
        let temp_degrees_c = 27.0 - (adc_voltage - 0.706) / 0.001721;

        if temp_degrees_c > 40.0 {
            // lerp from 55 to 65 degrees maps to gain from 1.0 to 0.1
            let gain: f64 = 1.0 - (temp_degrees_c - 55.0) / 10.0;
            let gain = gain.clamp(0.0, 1.0);
            control
                .send(AppCommand::ThermalThrottleMultiplier(gain as f32))
                .await;
        }

        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn button_driver(mut button: Input<'static>, control: AppSender) {
    let mut press_start;

    loop {
        button.wait_for_low().await;
        press_start = Instant::now();

        match with_timeout(Duration::from_millis(1000), button.wait_for_high()).await {
            // no timeout
            Ok(_) => {}
            // timeout
            Err(_) => {
                control.send(AppCommand::LongButtonPress).await;
                button.wait_for_high().await;
            }
        }

        let press_duration = Instant::now() - press_start;

        if press_duration >= Duration::from_millis(50)
            && press_duration < Duration::from_millis(1000)
        {
            control.send(AppCommand::ShortButtonPress).await;
        }
    }
}
