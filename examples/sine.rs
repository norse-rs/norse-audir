use norse_audir as audir;

fn main() {
    unsafe {
        let instance = audir::wasapi::Instance::create("audir - sine");

        let input_devices = instance.enumerate_physical_input_devices();
        let output_devices = instance.enumerate_physical_output_devices();

        println!(
            "I: {:?} / O: {:?}",
            input_devices.len(),
            output_devices.len()
        );

        for device in &output_devices {
            println!("{:#?}", device.get_properties());
        }

        let device = instance.create_device(&output_devices[0]);
        let stream = device.get_output_stream();
        let properties = dbg!(device.properties());

        let frequency = 100.0;
        let sample_rate = 48_000.0;
        let num_channels = properties.num_channels;
        let cycle_step = frequency / sample_rate;
        let mut cycle = 0.0;

        device.start();

        loop {
            let (raw_buffer, num_frames) = stream.acquire_buffer(!0);
            let buffer = std::slice::from_raw_parts_mut(
                raw_buffer as *mut f32,
                num_frames as usize * num_channels,
            );

            for dt in 0..num_frames {
                let phase = 2.0 * std::f32::consts::PI * cycle;
                let sample = phase.sin() * 0.5;

                buffer[num_channels * dt as usize] = sample;
                buffer[num_channels * dt as usize + 1] = sample;

                cycle = (cycle + cycle_step) % 1.0;
            }

            stream.submit_buffer(num_frames);
        }
    }
}
