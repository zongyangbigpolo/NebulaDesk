//! Bounded macOS system-audio → Opus → PCM diagnostic, not a playback test.
//!
//! Run in the authorized desktop session with `cargo run -p nebula-agent
//! --example audio_probe`. After capture starts, a separate `afplay` process
//! plays a quiet, generated 997 Hz tone for four seconds. No volume or device
//! settings are changed. Only the generated tone is written to disk (in the
//! current directory, then removed); captured PCM is measured, never saved.
//! Exit success requires the reference frequency in decoded Opus, not just
//! callbacks or nonzero samples. This does not test transport/client playback.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::io::Write;
    use std::time::{Duration, Instant};

    const RATE: u32 = 48_000;
    const HZ: f64 = 997.0;
    tracing_subscriber::fmt::init();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(35));
        eprintln!("audio probe exceeded its 35s deadline");
        std::process::exit(124);
    });

    struct Tone {
        path: std::path::PathBuf,
        child: Option<std::process::Child>,
    }
    impl Drop for Tone {
        fn drop(&mut self) {
            if let Some(child) = &mut self.child {
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    let config = nebula_agent::media::AudioConfig::default();
    let mut source = nebula_agent::platform::native().audio()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let started = Instant::now();
    source.start(config, tx)?;
    println!(
        "capture started after {:.3}s",
        started.elapsed().as_secs_f64()
    );

    let path =
        std::env::current_dir()?.join(format!(".nebula-audio-probe-{}.wav", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let mut tone = Tone { path, child: None };
    let frames = RATE * 4;
    let bytes = frames * 4;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&RATE.to_le_bytes())?;
    file.write_all(&(RATE * 4).to_le_bytes())?;
    file.write_all(&4u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    let mut pcm = Vec::with_capacity(bytes as usize);
    for frame in 0..frames {
        let sample = ((std::f64::consts::TAU * HZ * f64::from(frame) / f64::from(RATE)).sin()
            * 1600.0) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    file.write_all(&pcm)?;
    drop(file);
    tone.child = Some(
        std::process::Command::new("/usr/bin/afplay")
            .arg(&tone.path)
            .spawn()?,
    );
    println!("playing generated 997Hz reference at amplitude 0.049 for 4s");

    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
        let mut decoder = opus::Decoder::new(RATE, opus::Channels::Stereo)?;
        let mut decoded = vec![0.0f32; 5760 * 2];
        let mut packets = 0;
        let mut reference_packets = 0;
        let mut peak = 0.0f32;
        let mut energy = 0.0;
        let mut samples = 0usize;
        while let Ok(Some(packet)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            let frames = decoder.decode_float(&packet.data, &mut decoded, false)?;
            packets += 1;
            let mut left = Vec::with_capacity(frames);
            for stereo in decoded[..frames * 2].chunks_exact(2) {
                for &sample in stereo {
                    anyhow::ensure!(sample.is_finite(), "decoded non-finite PCM");
                    peak = peak.max(sample.abs());
                    energy += f64::from(sample).powi(2);
                    samples += 1;
                }
                left.push(stereo[0]);
            }
            if matches_reference(&left, RATE, HZ) {
                reference_packets += 1;
            }
        }
        let playback = tone.child.as_mut().unwrap().try_wait()?;
        println!(
            "packets={packets} samples={samples} peak={peak:.6} rms={:.6} reference_packets={reference_packets} afplay={playback:?}",
            (energy / samples.max(1) as f64).sqrt()
        );
        anyhow::ensure!(playback.is_some_and(|status| status.success()), "reference playback failed or stalled");
        anyhow::ensure!(reference_packets >= 50, "fewer than 1s of decoded 997Hz reference; capture not verified");
        Ok(())
    });
    source.stop();
    result
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("this controlled system-audio probe requires macOS");
}

#[cfg(any(target_os = "macos", test))]
fn matches_reference(samples: &[f32], rate: u32, hz: f64) -> bool {
    let mut energy = 0.0;
    let mut sine = 0.0;
    let mut cosine = 0.0;
    for (n, &sample) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * hz * n as f64 / f64::from(rate);
        let sample = f64::from(sample);
        energy += sample * sample;
        sine += sample * phase.sin();
        cosine += sample * phase.cos();
    }
    let count = samples.len() as f64;
    energy > count * 0.005f64.powi(2)
        && 2.0 * (sine * sine + cosine * cosine) > 0.8 * count * energy
}

#[cfg(test)]
mod tests {
    use super::matches_reference;

    #[test]
    fn reference_meter_rejects_silence_and_other_frequencies() {
        let tone = |hz: f64| {
            (0..960)
                .map(|n| {
                    (0.04 * (std::f64::consts::TAU * hz * f64::from(n) / 48_000.0).sin()) as f32
                })
                .collect::<Vec<_>>()
        };
        assert!(matches_reference(&tone(997.0), 48_000, 997.0));
        assert!(!matches_reference(&tone(440.0), 48_000, 997.0));
        assert!(!matches_reference(&[0.0; 960], 48_000, 997.0));
        assert!(!matches_reference(&[], 48_000, 997.0));
    }
}
