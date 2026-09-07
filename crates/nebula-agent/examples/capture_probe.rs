//! Prints what the capture pipeline actually produces, for checking a machine
//! before it is enrolled. Run inside the desktop session with capture permission
//! and a supported hardware encoder.
fn main() -> anyhow::Result<()> {
    use nebula_agent::media::VideoConfig;
    tracing_subscriber::fmt::init();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut video = nebula_agent::platform::native().video()?;
    video.start(
        VideoConfig {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate: 8_000_000,
        },
        tx,
    )?;
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        for n in 0..30u32 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Some(f)) => println!(
                    "frame {n}: {} bytes, keyframe={}, t={}us",
                    f.data.len(),
                    f.keyframe,
                    f.timestamp_us
                ),
                Ok(None) => {
                    anyhow::bail!("capture stopped before the probe completed");
                }
                Err(_) => {
                    if n == 0 {
                        anyhow::bail!("capture started but produced no frame within 5s");
                    }
                    println!("no frame within 5s");
                    break;
                }
            }
        }
        Ok(())
    });
    video.stop();
    result
}
