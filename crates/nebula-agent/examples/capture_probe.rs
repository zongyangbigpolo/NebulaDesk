//! Prints what the capture pipeline actually produces, for checking a machine
//! before it is enrolled. Needs Screen Recording permission.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use nebula_agent::media::{VideoConfig, VideoSource};
    tracing_subscriber::fmt::init();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut video = nebula_agent::platform::macos::capture::MacVideo::new();
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
    rt.block_on(async {
        for n in 0..30u32 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Some(f)) => println!(
                    "frame {n}: {} bytes, keyframe={}, t={}us",
                    f.data.len(),
                    f.keyframe,
                    f.timestamp_us
                ),
                Ok(None) => {
                    println!("capture stopped");
                    break;
                }
                Err(_) => {
                    println!("no frame within 5s");
                    break;
                }
            }
        }
    });
    video.stop();
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}
