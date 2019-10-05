use std::env;

use std::path::Path;

#[cfg(target_os = "android")]
pub fn load<P: AsRef<Path>>(path: P) -> Vec<u8> {
    use android_glue;

    let filename = path.as_ref().to_str().expect("Can`t convert Path to &str");
    match android_glue::load_asset(filename) {
        Ok(buf) => buf,
        Err(_) => panic!("Can`t load asset '{}'", filename),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_min_level(log::Level::Trace) // limit log level
                .with_tag("audir"), // logs will show under mytag tag
        );
    }

    log::warn!("start");

    #[cfg(not(target_os = "android"))]
    let mut audio_stream = {
        let file_path = env::args()
            .nth(1)
            .expect("No arg found. Please specify a file to open.");
        audrey::open(file_path)?
    };

    #[cfg(target_os = "android")]
    let mut audio_stream = {
        let file = std::io::Cursor::new(load("asmr_48000.ogg"));
        audrey::Reader::new(file).unwrap()
    };

    let samples = audio_stream
        .frames::<[f32; 2]>()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    log::warn!("samples: {}", samples.len());

    unsafe {
        #[cfg(windows)]
        let instance = audir::wasapi::Instance::create("audir - music");
        #[cfg(target_os = "linux")]
        let instance = audir::pulse::Instance::create("audir - music");
        #[cfg(target_os = "android")]
        let instance = audir::opensles::Instance::create("audir - music");

        let output_devices = instance.enumerate_physical_output_devices();
        let device = instance.create_device(
            &output_devices[0],
            audir::SampleDesc {
                format: audir::Format::F32,
                channels: 2,
                sample_rate: 48_000,
            },
        );

        let properties = dbg!(device.properties());

        let sample_rate = properties.sample_rate;
        let num_channels = properties.num_channels;

        let mut stream = device.output_stream();
        stream.start();

        let mut sample = 0;
        loop {
            let (raw_buffer, num_frames) = stream.acquire_buffer(!0);
            let buffer = std::slice::from_raw_parts_mut(
                raw_buffer as *mut f32,
                num_frames as usize * num_channels,
            );

            for dt in 0..num_frames as usize {
                let frame = samples[sample];
                buffer[num_channels * dt as usize] = frame[0];
                buffer[num_channels * dt as usize + 1] = frame[1];
                sample = (sample + 1) % samples.len();
            }

            stream.submit_buffer(num_frames);
        }
    }

    Ok(())
}
