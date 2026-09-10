//! The media path, end to end, on one machine.
//!
//! Capture a real screen, encode it in hardware, and decode it back with the
//! code the client actually ships. That covers the seam nothing else can:
//! AVCC framing, parameter sets riding in front of keyframes, and a decoder
//! built from the stream rather than from anything agreed out of band.
//!
//! Ignored by default. It needs an interactive desktop, capture permission,
//! and hardware H.264 encoding/decoding. A headless CI build does not establish
//! any of those. Run it by hand on each supported platform:
//!
//! ```text
//! cargo test -p nebula-client --test media -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]

use std::time::Duration;

use nebula_agent::media::VideoConfig;

#[test]
#[ignore = "needs an interactive desktop, capture permission and hardware codecs"]
fn a_captured_screen_decodes_back_into_a_picture() {
    let (frames_tx, mut frames) = tokio::sync::mpsc::channel(8);
    let mut video = nebula_agent::platform::native()
        .video()
        .expect("the native platform must provide a video source");
    video
        .start(
            VideoConfig {
                width: 1280,
                height: 720,
                fps: 30,
                bitrate: 6_000_000,
            },
            frames_tx.into(),
        )
        .expect(
            "capture must start; check the platform's desktop, permission and GPU requirements",
        );

    let mut decoder = nebula_client::video::decoder().expect("the client must have a decoder");
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let first = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(15), frames.recv()).await })
        .expect("the encoder should produce a frame")
        .expect("the capture pipeline should not close");

    // Whatever a client sees first has to stand alone, because there is
    // nothing before it to reference.
    assert!(
        first.keyframe,
        "the first frame of a session must be a keyframe"
    );

    let picture = decoder
        .decode(&first.data)
        .expect("the first keyframe must decode")
        .expect("a keyframe carrying its own parameter sets must produce a picture");

    assert!(
        picture.width >= 2 && picture.height >= 2,
        "decoded {}x{}",
        picture.width,
        picture.height
    );
    assert_eq!(
        picture.y.len(),
        (picture.width * picture.height) as usize,
        "the luma plane must be exactly the picture, with no row padding left in"
    );
    let chroma = (picture.width.div_ceil(2) * picture.height.div_ceil(2)) as usize;
    assert_eq!(picture.u.len(), chroma);
    assert_eq!(picture.v.len(), chroma);

    // A picture of one flat value is what a decode failure looks like when it
    // does not report itself: all black, all green, or all grey.
    let distinct = picture
        .y
        .iter()
        .step_by(97)
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert!(
        distinct > 1,
        "the decoded picture is a single flat value, which means nothing real was decoded"
    );

    video.stop();
}
