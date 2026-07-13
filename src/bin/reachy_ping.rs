use rustypot::DynamixelProtocolHandler;
use std::time::Duration;

fn main() {
    let port_name = "/dev/cu.usbmodem5AF71345631";
    let baud = 1_000_000;

    println!("Opening port {} at {} baud...", port_name, baud);
    let mut serial_port = serialport::new(port_name, baud)
        .timeout(Duration::from_millis(10))
        .open()
        .expect("Failed to open port");

    let mut dph_v1 = DynamixelProtocolHandler::v1();
    let mut dph_v2 = DynamixelProtocolHandler::v2();

    println!("Scanning for servos using V1 Protocol...");
    for id in 1..=40 {
        if let Ok(true) = dph_v1.ping(serial_port.as_mut(), id) {
            println!("Found motor on V1 at ID: {}", id);
        }
    }

    println!("Scanning for servos using V2 Protocol...");
    for id in 1..=40 {
        if let Ok(true) = dph_v2.ping(serial_port.as_mut(), id) {
            // Read model number (address 0, 2 bytes)
            if let Ok(model) = rustypot::servo::dynamixel::xl320::read_model_number(
                &dph_v2,
                serial_port.as_mut(),
                id,
            ) {
                println!("Found motor on V2 at ID: {}, Model: {}", id, model);
            } else {
                println!("Found motor on V2 at ID: {}, Model: unknown", id);
            }
        }
    }

    println!("Scan complete.");
}
