#![no_std]
#![no_main]

mod rotary;

use num_traits::float::FloatCore;
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use defmt::{info, unwrap};
use display_interface_spi::SPIInterface;
use embassy_executor::{Executor, Spawner};
use embassy_rp::gpio::Level::{High, Low};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{SPI0, SPI1};
use embassy_rp::{pwm, spi};
use embassy_rp::spi::{Async, Phase, Polarity, Spi};
use embedded_alloc::Heap;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::mutex::Mutex;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};
use variegated_embassy_ads124s08::{WaitStrategy, ADS124S08};
use variegated_hal::{Boiler, DutyCycleType, FlowRateType, Group, PressureType, RPMType, TemperatureType, WithTask};
use variegated_hal::gpio::gpio_binary_heating_element::{GpioBinaryHeatingElement, GpioBinaryHeatingElementControl};
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_futures::join::{join, join3, join4, join5, join_array};
use embassy_futures::select::Either::{First, Second};
use embassy_futures::select::select;
use embassy_rp::pwm::InputMode;
use embassy_sync::channel::{Channel, Receiver};
use embassy_sync::signal::Signal;
use embassy_sync::watch::{Watch};
use embassy_time::{Delay, Timer};
use embedded_graphics::primitives::{PrimitiveStyleBuilder, StyledDrawable};
use embedded_graphics_core::primitives::Rectangle;
use embedded_graphics_core::prelude::*;
use rotary_encoder_hal::Rotary;
use variegated_adc_tools::ConversionParameters;
use variegated_controller_lib::{BoilerControlTarget, GroupBrewControlTarget, MachineCommand, PidParameters, PidTerm, SingleBoilerSingleGroupController, Status};
use variegated_embassy_ads124s08::registers::{IDACMagnitude, IDACMux, Mux, PGAGain, ReferenceInput};
use variegated_embassy_ads124s08::registers::SystemMonitorConfiguration::DvddBy4Measurement;
use variegated_hal::adc::ads124s08::Ads124S08Sensor;
use variegated_hal::adc::ads124s08::MeasurementType::{AvddBy4, DvddBy4, RatiometricLowSide, SingleEnded};
use variegated_hal::machine_mechanism::single_boiler_mechanism::{SingleBoilerBrewMechanism, SingleBoilerMechanism};

use embedded_graphics::{
    mono_font::{ascii::FONT_5X7, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Baseline, Text},
};
use embedded_hal::pwm::SetDutyCycle;
use oled_async::{displays, prelude::*, Builder};
use variegated_hal::gpio::gpio_command_sender::GpioDualEdgeCommandSender;
use variegated_hal::gpio::gpio_pwm_frequency_counter::GpioTransformingFrequencyCounter;
use variegated_hal::gpio::gpio_three_way_solenoid::GpioThreeWaySolenoid;
use crate::rotary::{UIEditMode, UIStatus};

#[global_allocator]
static HEAP: Heap = Heap::empty();

#[variegated_board_cfg::board_cfg("display_peripherals")]
struct DisplayPeripherals {
    spi: (),
    sclk_pin: (),
    mosi_pin: (),
    miso_pin: (),
    cs_pin: (),
    dc_pin: (),
    rst_pin: (),
    dma_tx: (),
    dma_rx: (),
}

#[variegated_board_cfg::board_cfg("internal_spi_bus_peripherals")]
struct InternalSpiBusPeripherals {
    spi: (),
    sclk_pin: (),
    mosi_pin: (),
    miso_pin: (),
    dma_tx: (),
    dma_rx: (),
}

#[variegated_board_cfg::board_cfg("rotary_encoder_peripherals")]
struct RotaryEncoderPeripherals {
    pin_clk: (),
    pin_dt: (),
    pin_sw: (),
}

#[variegated_board_cfg::board_cfg("ads124s08_peripherals")]
struct Ads124S08Peripherals {
    pin_drdy: (),
    pin_cs: (),
}

#[variegated_board_cfg::board_cfg("button_peripherals")]
struct ButtonPeripherals {
    pin_brew: (),
    pin_water: (),
    pin_steam: (),
}

#[variegated_board_cfg::board_cfg("pump_peripherals")]
struct PumpPeripherals {
    pwm_speed: (),
    pin_speed: (),
    pwm_tacho_out: (),
    pin_tacho_out: (),
    pin_dir: (),
}

#[variegated_board_cfg::board_cfg("flow_meter_peripherals")]
struct FlowMeterPeripherals {
    pwm_flow_meter: (),
    pin_flow_meter: (),
}

#[variegated_board_cfg::board_cfg("mechanism_peripherals")]
struct MechanismPeripherals {
    pin_he: (),
    pin_solenoid: (),
}

type DisplayBus = Mutex<NoopRawMutex, Spi<'static, DisplayPeripheralsSpi, spi::Async>>;
type InternalBus = Mutex<NoopRawMutex, Spi<'static, InternalSpiBusPeripheralsSpi, spi::Async>>;
type AdsMutex = Mutex<NoopRawMutex, ADS124S08<SpiDevice<'static, NoopRawMutex, Spi<'static, InternalSpiBusPeripheralsSpi, Async>, Output<'static>>, Input<'static>>>;

static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
#[cortex_m_rt::entry]
fn main() -> ! {
    {
        use core::mem::MaybeUninit;
        const HEAP_SIZE: usize = 1024;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE) }
    }

    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| {
        unwrap!(spawner.spawn(main_task(spawner)))
    });
}

static SPI_BUS: StaticCell<InternalBus> = StaticCell::new();
static ADS: StaticCell<AdsMutex> = StaticCell::new();
static TEMP_SIGNAL: StaticCell<Watch<NoopRawMutex, TemperatureType, 3>> = StaticCell::new();
static PRESSURE_SIGNAL: StaticCell<Watch<NoopRawMutex, PressureType, 3>> = StaticCell::new();
static HE_SIGNAL: StaticCell<Signal<CriticalSectionRawMutex, DutyCycleType>> = StaticCell::new();
static PUMP_RPM_SIGNAL: StaticCell<Watch<NoopRawMutex, RPMType, 3>> = StaticCell::new();
static FLOW_SIGNAL: StaticCell<Watch<NoopRawMutex, FlowRateType, 3>> = StaticCell::new();
static MECHANISM_MUTEX: StaticCell<Mutex<CriticalSectionRawMutex, SingleBoilerMechanism>> = StaticCell::new();
static COMMAND_CHANNEL: StaticCell<Channel<CriticalSectionRawMutex, MachineCommand, 10>> = StaticCell::new();
static STATUS_CHANNEL: StaticCell<Channel<CriticalSectionRawMutex, Status, 10>> = StaticCell::new();
static UI_STATUS_CHANNEL: StaticCell<Channel<CriticalSectionRawMutex, UIStatus, 10>> = StaticCell::new();

#[embassy_executor::task]
async fn main_task(spawner: Spawner) -> ! {
    let p = embassy_rp::init(Default::default());

    Timer::after_millis(2000).await;
    defmt::info!("Starting!");
    
    // Shared SPI bus
    let mut spi_config = spi::Config::default();
    spi_config.frequency = 281_000;
    spi_config.phase = Phase::CaptureOnSecondTransition;
    spi_config.polarity = Polarity::IdleLow;

    let spi_p = internal_spi_bus_peripherals!(p);
    let ads_p = ads_124s08_peripherals!(p);

    let mut spi = Spi::new(spi_p.spi, spi_p.sclk_pin, spi_p.mosi_pin, spi_p.miso_pin, spi_p.dma_tx, spi_p.dma_rx, spi_config);
    let spi_bus = SPI_BUS.init(Mutex::new(spi));
    let spi_dev = SpiDevice::new(spi_bus, Output::new(ads_p.pin_cs, High));
    
    let mut ads = ADS124S08::new(spi_dev, WaitStrategy::UseDrdyPin(Input::new(ads_p.pin_drdy, Pull::Down)));
    info!("Resetting ADS124S08");
    let res = ads.reset().await;
    if let Err(e) = res {
        info!("Error resetting ADS124S08: {:?}", e);
    }
    info!("Done");
    
    let ads = ADS.init(Mutex::new(ads));
    
    let temp_sig: &'static Watch<_, _, 3>  = TEMP_SIGNAL.init(Watch::new());
    let mut temp_sensor = Ads124S08Sensor::new(
        ads,
        temp_sig.sender(),
        RatiometricLowSide(Mux::AIN1, Mux::AIN2, IDACMux::AIN0, IDACMux::AIN3, ReferenceInput::Refp0Refn0, IDACMagnitude::Mag1000uA, PGAGain::Gain4, 1620.0),
        ConversionParameters::pt100(),
        -2.95
    );

    let prs_sig: &'static Watch<_, _, 3> = PRESSURE_SIGNAL.init(Watch::new());
    let mut pressure_sensor = Ads124S08Sensor::new(
        ads,
        prs_sig.sender(),
        SingleEnded(
            Mux::AIN4,
            ReferenceInput::Refp1Refn1,
            5.0
        ),
        ConversionParameters::linear_range_mapping(0.5, 4.5, 0.0, 15.0),
        0.0
    );

    let mechanism_p = mechanism_peripherals!(p);

    let sig: &'static Signal<_, _> = HE_SIGNAL.init(Signal::new());

    let mut he = GpioBinaryHeatingElement::new(Output::new(mechanism_p.pin_he, Low), sig);
    let he_control = GpioBinaryHeatingElementControl::new(sig);

    let boiler = Boiler::new(
        Box::new(he_control),
        None,
        Some(temp_sig.receiver().unwrap()),
        Some(prs_sig.receiver().unwrap()),
        None
    );

    let pump_p = pump_peripherals!(p);

    let pump_dir = Output::new(pump_p.pin_dir, Low);

    info!("System clock: {:?}", embassy_rp::clocks::clk_sys_freq());

    let mut pwm_config = pwm::Config::default();
    // 10 KHz, assuming a system clock of 150 MHz, which is the default on the RP2350B
    pwm_config.divider = 1.into();
    pwm_config.top = 14999;
    let (pump_pwm, _) = pwm::Pwm::new_output_a(pump_p.pwm_speed, pump_p.pin_speed, pwm_config).split();
    let mut pump_pwm = pump_pwm.unwrap();

    let mut pwm_input_config = pwm::Config::default();
    pwm_input_config.divider = 1.into();
    let input = pwm::Pwm::new_input(pump_p.pwm_tacho_out, pump_p.pin_tacho_out, Pull::Up, InputMode::FallingEdge, pwm_input_config);

    let pump_rpm_sig: &'static Watch<_, _, 3> = PUMP_RPM_SIGNAL.init(Watch::new());
    let mut pump_frequency_counter = GpioTransformingFrequencyCounter::new(input, pump_rpm_sig.sender(), |v| (v * 60.0/32.0) as RPMType);

    let pump = variegated_hal::gpio::gpio_pwm_pump::GpioPwmPump::new(pump_pwm);

    let solenoid_output = Output::new(mechanism_p.pin_solenoid, Low);
    let solenoid = GpioThreeWaySolenoid::new(solenoid_output);

    let mechanism = SingleBoilerMechanism::new(pump, solenoid);
    let mechanism_mutex: &Mutex<_, _> = MECHANISM_MUTEX.init(Mutex::new(mechanism));
    let brew_mechanism = SingleBoilerBrewMechanism::new(mechanism_mutex);

    let flow_meter_p = flow_meter_peripherals!(p);

    let mut pwm_input_config = pwm::Config::default();
    pwm_input_config.divider = 1.into();
    let flow_meter_input = pwm::Pwm::new_input(flow_meter_p.pwm_flow_meter, flow_meter_p.pin_flow_meter, Pull::Up, InputMode::FallingEdge, pwm_input_config);

    let flow_meter_sig: &'static Watch<_, _, 3> = FLOW_SIGNAL.init(Watch::new());
    let mut flow_meter = GpioTransformingFrequencyCounter::new(flow_meter_input, flow_meter_sig.sender(), |v| (v * 0.043) * 0.6667 * 0.89 as FlowRateType);

    let group = Group::new(
        Some(Box::new(brew_mechanism)),
        None,
        None,
        Some(prs_sig.receiver().unwrap()),
        Some(flow_meter_sig.receiver().unwrap()),
    );

    let command_channel: &'static Channel<_, _, 10> = COMMAND_CHANNEL.init(Channel::new());
    let status_channel: &'static Channel<_, _, 10> = STATUS_CHANNEL.init(Channel::new());

    // Output is always in percent
    let group_flow_rate_params = PidParameters {
        kp: PidTerm { scale: 3.0, min: None, max: None },
        ki: PidTerm { scale: 0.01, min: Some(-80.0), max: Some(80.0) },
        kd: PidTerm { scale: 30.0, min: None, max: None }
    };

    // Output is always in percent
    let pressure_params = PidParameters {
        kp: PidTerm { scale: 3.0, min: None, max: None },
        ki: PidTerm { scale: 0.01, min: Some(-10.0), max: Some(10.0) },
        kd: PidTerm { scale: 30.0, min: Some(-10.0), max: Some(10.0) }
    };

    let mut controller = SingleBoilerSingleGroupController::new(
        command_channel.receiver(),
        status_channel.sender(),
        boiler,
        group,
        BoilerControlTarget::Off,
        BoilerControlTarget::Off,
        GroupBrewControlTarget::FixedDutyCycle(30)
        //GroupBrewControlTarget::Pressure(3.0, pressure_params),
        //GroupBrewControlTarget::GroupFlowRate(5.0, group_flow_rate_params)
    );

    let button_p = button_peripherals!(p);
    let rotary_p = rotary_encoder_peripherals!(p);

    let mut brew_action = GpioDualEdgeCommandSender::new(
        Input::new(button_p.pin_brew, Pull::Up),
        command_channel.sender(),
        MachineCommand::StopBrewing,
        MachineCommand::StartBrewing,
    );

    let ui_status_channel: &'static Channel<_, _, 10> = UI_STATUS_CHANNEL.init(Channel::new());

    let mut rotary_action = rotary::RotaryController::new(
        Rotary::new(Input::new(rotary_p.pin_dt, Pull::Up), Input::new(rotary_p.pin_clk, Pull::Up)),
        Input::new(rotary_p.pin_sw, Pull::Up),
        command_channel.sender(),
        ui_status_channel.sender(),
    );

    let disp_p = display_peripherals!(p);

    spawner.spawn(display_task(disp_p, status_channel.receiver(), ui_status_channel.receiver())).unwrap();

    join5(
        join4(
            temp_sensor.task(),
            brew_action.task(),
            flow_meter.task(),
            rotary_action.task(),
        ),
        pressure_sensor.task(),
        he.task(),
        pump_frequency_counter.task(),
        controller.task()
    ).await;
    
    loop {
        Timer::after_millis(3000).await;
    }
}
fn launch_button_and_rotary_tasks(spawner: &Spawner, button_p: ButtonPeripherals) {
    let brew_button = Input::new(button_p.pin_brew, Pull::Up);
    let water_button = Input::new(button_p.pin_water, Pull::Up);
    let steam_button = Input::new(button_p.pin_steam, Pull::Up);

    spawner.spawn(button_task(brew_button, "Brew button")).unwrap();
    spawner.spawn(button_task(water_button, "Water button")).unwrap();
    spawner.spawn(button_task(steam_button, "Steam button")).unwrap();
}

#[embassy_executor::task]
async fn display_task(
    disp_p: DisplayPeripherals,
    status_receiver: Receiver<'static, CriticalSectionRawMutex, Status, 10>,
    ui_status_receiver: Receiver<'static, CriticalSectionRawMutex, UIStatus, 10>
) {
    let spi_config = spi::Config::default();
    let mut spi = Spi::new(
        disp_p.spi,
        disp_p.sclk_pin,
        disp_p.mosi_pin,
        disp_p.miso_pin,
        disp_p.dma_tx,
        disp_p.dma_rx,
        spi_config
    );
    static SPI0_BUS: StaticCell<DisplayBus> = StaticCell::new();
    let spi_bus = SPI0_BUS.init(Mutex::new(spi));
    let spi_dev = SpiDevice::new(spi_bus, Output::new(disp_p.cs_pin, High));

    let dc = Output::new(disp_p.dc_pin, Low);
    let mut res = Output::new(disp_p.rst_pin, Low);

    let di = SPIInterface::new(spi_dev, dc);

    let raw_disp = Builder::new(displays::ssd1309::Ssd1309_128_64 {})
        .with_rotation(DisplayRotation::Rotate0)
        .connect(di);

    let mut disp: GraphicsMode<_, _> = raw_disp.into();
    disp.reset(&mut res, &mut Delay {}).expect("Failed to reset display");
    disp.init().await.expect("Failed to initialize display");
    disp.flush().await.expect("Failed to flush display");
    disp.clear();

    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_5X7)
        .text_color(BinaryColor::On)
        .build();

    Text::with_baseline("Hello world!", Point::zero(), text_style, Baseline::Top)
        .draw(&mut disp)
        .unwrap();

    disp.flush().await.expect("Failed to flush display second time");

    let mut status = Status::default();
    let mut ui_status = UIStatus::default();

    loop {
        if let Ok(status_update) = status_receiver.try_receive() {
            status = status_update;
        }

        if let Ok(ui_status_update) = ui_status_receiver.try_receive() {
            ui_status = ui_status_update;
        }

        disp.clear();

        let target_temp = match status.config_brew_boiler_control_target {
            BoilerControlTarget::Off => 0.0,
            BoilerControlTarget::Temperature(temp, ..) => temp,
            BoilerControlTarget::Pressure(_, ..) => 0.0,
        };

        Text::with_baseline(format!("T: {:.2} C (Tgt {:.0})", status.boiler_temp, target_temp).as_str(), Point::zero(), text_style, Baseline::Top)
            .draw(&mut disp)
            .unwrap();

        let pump_dc = match status.config_group_brew_control_target {
            GroupBrewControlTarget::FixedDutyCycle(dc) => dc,
            _ => 0
        };

        if let Some(pressure) = status.boiler_pressure {
            Text::with_baseline(format!("P: {:.2} bar (PT {:.0}%)", pressure, pump_dc).as_str(), Point::new(0, 7), text_style, Baseline::Top)
                .draw(&mut disp)
                .unwrap();
        }

        if let Some(flow_rate) = status.group_flow_rate {
            Text::with_baseline(format!("Flow: {:.1} ml/s", flow_rate).as_str(), Point::new(0, 14), text_style, Baseline::Top)
                .draw(&mut disp)
                .unwrap();
        }

        Text::with_baseline(format!("Pump: {:.0} % Boil: {:.0}%", status.pump_duty_cycle, status.brew_boiler_duty_cycle).as_str(), Point::new(0, 21), text_style, Baseline::Top)
            .draw(&mut disp)
            .unwrap();

        if let Some(boiler_pid) = status.boiler_pid_output {
            Text::with_baseline(format!("Boil P: {:.0} I: {:.0} D: {:.0}", boiler_pid.p, boiler_pid.i, boiler_pid.d).as_str(), Point::new(0, 28), text_style, Baseline::Top)
                .draw(&mut disp)
                .unwrap();
        }

        if let Some(pump_pid) = status.pump_pid_output {
            Text::with_baseline(format!("Pump P: {:.0} I: {:.0} D: {:.0}", pump_pid.p, pump_pid.i, pump_pid.d).as_str(), Point::new(0, 35), text_style, Baseline::Top)
                .draw(&mut disp)
                .unwrap();
        }

        match ui_status.edit_mode {
            UIEditMode::PumpDutyCycle => {
                Text::with_baseline(format!("Edit: Pump DC ({:.0})", ui_status.current_duty_cycle).as_str(), Point::new(0, 42), text_style, Baseline::Top)
                    .draw(&mut disp)
                    .unwrap();
            }
            UIEditMode::BoilerTemperature => {
                Text::with_baseline(format!("Edit: Boil T ({:.0})", ui_status.current_boiler_temp).as_str(), Point::new(0, 42), text_style, Baseline::Top)
                    .draw(&mut disp)
                    .unwrap();
            },
            UIEditMode::PumpFlowRate => {
                Text::with_baseline(format!("Edit: Flow ({:.0})", ui_status.current_flow_rate).as_str(), Point::new(0, 42), text_style, Baseline::Top)
                    .draw(&mut disp)
                    .unwrap();
            },
            UIEditMode::PumpPressure => {
                Text::with_baseline(format!("Edit: Prs ({:.0})", ui_status.current_pressure).as_str(), Point::new(0, 42), text_style, Baseline::Top)
                    .draw(&mut disp)
                    .unwrap();
            }

        };

        disp.flush().await.expect("Failed to flush display");

        Timer::after_millis(15).await;
    }
}

#[embassy_executor::task(pool_size = 4)]
async fn button_task(mut button: Input<'static>, button_name: &'static str) {
    loop {
        button.wait_for_any_edge().await;
        Timer::after_millis(3).await; // Debounce delay

        if button.is_low() {
            info!("{} pressed", button_name);
        } else {
            info!("{} released", button_name);
        }
    }
}

#[embassy_executor::task]
async fn temp_sensor_task(brew: Input<'static>, steam: Input<'static>, water: Input<'static>) {
    loop {
        if brew.is_low() {
            info!("Brew button pressed");
        }
        if steam.is_low() {
            info!("Steam button pressed");
        }
        if water.is_low() {
            info!("Water button pressed");
        }
        Timer::after_millis(1000).await;
    }
}