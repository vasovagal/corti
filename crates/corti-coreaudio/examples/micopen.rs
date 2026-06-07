//! Test helper: open the default input device for a few seconds so the mic-in-use trigger fires.
//!
//! Run the `probe` binary in one terminal and this in another; the probe should print `MIC ON` then
//! `mic off`. Used to exercise the detection path without a real conferencing app.
//!
//! ```sh
//! cargo run -p corti-coreaudio --example micopen
//! ```

use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn main() -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
    // cpal 0.18 deprecated/removed `Device::name()`; `Device` now implements `Display`, so the
    // human-readable device name is just its `{}` representation.
    println!("opening mic: {device}");
    let config = device.default_input_config()?;
    let stream = device.build_input_stream_raw(
        // cpal 0.18: `build_*_stream` take `StreamConfig` by value (it is now `Copy`), not `&`.
        config.config(),
        config.sample_format(),
        move |_data, _info: &cpal::InputCallbackInfo| {},
        move |err| eprintln!("stream error: {err}"),
        None,
    )?;
    stream.play()?;
    println!("mic held open for 4s…");
    std::thread::sleep(Duration::from_secs(4));
    println!("closing mic");
    Ok(())
}
