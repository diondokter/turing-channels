#![no_main]
#![no_std]
#![feature(type_alias_impl_trait)]

// links in a minimal version of libc
extern crate tinyrlibc;

use embassy::time::{Duration, Timer};
use heapless::String;
use pubsub::Subscriber;
use rtt_target::{rprintln, rtt_init_print};
use core::fmt::Write;

pub mod pubsub;

static MESSAGE_BUS: pubsub::PubSubChannel<embassy::blocking_mutex::raw::ThreadModeRawMutex, String<128>, 4, 2, 1> = pubsub::PubSubChannel::new();

#[embassy::main]
async fn main(spawner: embassy::executor::Spawner, p: embassy_nrf::Peripherals) {
    rtt_init_print!();

    let mut led_blue = embassy_nrf::gpio::Output::new(
        p.P0_03,
        embassy_nrf::gpio::Level::Low,
        embassy_nrf::gpio::OutputDrive::Standard,
    );

    rprintln!("LEDS");

    spawner.must_spawn(logger_0(MESSAGE_BUS.subscriber().unwrap()));
    spawner.must_spawn(logger_1(MESSAGE_BUS.subscriber().unwrap()));

    let mut message_publisher = MESSAGE_BUS.publisher().unwrap();

    let mut index = 0;
    loop {
        led_blue.set_high();
        Timer::after(Duration::from_millis(300)).await;
        led_blue.set_low();
        Timer::after(Duration::from_millis(300)).await;

        let mut message = String::new();
        write!(&mut message, "Current index: {index}").unwrap();
        message_publisher.publish(message).await;

        index += 1;
    }
}

#[embassy::task]
async fn logger_0(mut messages: Subscriber<'static, String<128>>) {
    loop {
        let message = messages.wait().await;
        rprintln!("Received message at 0: {:?}", message);
    }
}

#[embassy::task]
async fn logger_1(mut messages: Subscriber<'static, String<128>>) {
    let mut index = 0;
    loop {
        let message = messages.wait().await;
        rprintln!("Received message at 1: {:?}", message);

        if index % 10 == 0 {
            Timer::after(Duration::from_millis(300 * 20)).await;
        }

        index += 1;
    }
}


#[link_section = ".spm"]
#[used]
static SPM: [u8; 24052] = *include_bytes!("zephyr.bin");

#[cortex_m_rt::exception]
unsafe fn HardFault(frame: &cortex_m_rt::ExceptionFrame) -> ! {
    rprintln!("{:?}", frame);
    loop {
        cortex_m::asm::bkpt();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    rprintln!("{}", info);
    cortex_m::asm::udf();
}
