//! Putting decoded frames on the screen.
//!
//! The three planes go to the GPU as three single-channel textures and the
//! colour conversion happens in the fragment shader. Doing it on the CPU
//! would mean touching every pixel of every frame twice — once to convert and
//! once to upload — which at 4K60 is more memory traffic than the decode
//! itself.

use std::sync::Arc;

use wgpu::util::DeviceExt;

use crate::video::Picture;

/// BT.709 limited range, which is what every screen encoder produces.
///
/// Getting this wrong does not look broken, it looks slightly washed out or
/// slightly crushed, which is why it is written down rather than inferred.
const SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One oversized triangle rather than two triangles: it covers the viewport
// with three vertices and no seam down the diagonal.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    out.uv = vec2<f32>(x, y);
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma_u: texture_2d<f32>;
@group(0) @binding(2) var chroma_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@fragment
fn fs(in: VertexOutput) -> @location(0) vec4<f32> {
    // Limited range: luma occupies 16..235 and chroma 16..240 of 255.
    let y = (textureSample(luma, samp, in.uv).r - 0.0625) * 1.164383;
    let u = textureSample(chroma_u, samp, in.uv).r - 0.5;
    let v = textureSample(chroma_v, samp, in.uv).r - 0.5;

    // BT.709.
    let r = y + 1.792741 * v;
    let g = y - 0.213249 * u - 0.532909 * v;
    let b = y + 2.112402 * u;
    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// Draws pictures into a window.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<Planes>,
}

/// The textures holding the most recent picture.
struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind: wgpu::BindGroup,
}

impl Renderer {
    /// Attach to a window and prepare the pipeline.
    pub async fn new(
        window: Arc<dyn wgpu::WindowHandle>,
        size: (u32, u32),
    ) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no usable GPU was found"))?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("nebula"),
                    required_features: wgpu::Features::empty(),
                    // The default limits are what every target this ships to
                    // supports; asking for more would exclude machines for the
                    // sake of drawing one textured triangle.
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await?;

        let capabilities = surface.get_capabilities(&adapter);
        // A non-sRGB surface on purpose. The shader below already emits
        // gamma-encoded values, because that is what BT.709 video carries; an
        // sRGB surface would apply the transfer function a second time on
        // write and wash the whole picture out.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .unwrap_or_else(|| capabilities.formats[0].remove_srgb_suffix());
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            // Fifo everywhere: it is the only mode guaranteed to exist, and
            // tearing on a desktop stream looks worse than one frame of
            // latency feels.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("planes"),
            entries: &[
                plane_binding(0),
                plane_binding(1),
                plane_binding(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nebula"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nebula"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("bilinear"),
            // Bilinear, and clamped: the picture is scaled to the window, and
            // repeating would wrap the far edge into view along the seam.
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            planes: None,
        })
    }

    /// Follow the window's new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            // A minimised window has no surface to configure, and asking for
            // a zero-sized one is a validation error on every backend.
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// The size of the picture currently loaded, if any.
    #[must_use]
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(|p| (p.width, p.height))
    }

    /// Upload a decoded picture, replacing whatever was shown before.
    pub fn upload(&mut self, picture: &Picture) {
        if picture.width == 0 || picture.height == 0 {
            return;
        }
        let (cw, ch) = (picture.width.div_ceil(2), picture.height.div_ceil(2));

        // Textures are only rebuilt when the resolution changes. A session
        // holds one size for minutes at a time, so reallocating per frame
        // would be pure waste.
        let stale = self
            .planes
            .as_ref()
            .is_none_or(|p| p.width != picture.width || p.height != picture.height);
        if stale {
            let y = self.plane_texture(picture.width, picture.height, &picture.y);
            let u = self.plane_texture(cw, ch, &picture.u);
            let v = self.plane_texture(cw, ch, &picture.v);
            let bind = self.bind(&y, &u, &v);
            self.planes = Some(Planes {
                width: picture.width,
                height: picture.height,
                y,
                u,
                v,
                bind,
            });
            return;
        }

        let planes = self.planes.as_ref().expect("checked above");
        write(
            &self.queue,
            &planes.y,
            picture.width,
            picture.height,
            &picture.y,
        );
        write(&self.queue, &planes.u, cw, ch, &picture.u);
        write(&self.queue, &planes.v, cw, ch, &picture.v);
    }

    /// Draw the current picture, letterboxed to keep its shape.
    pub fn draw(&mut self) -> anyhow::Result<()> {
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            // The surface goes stale when a window is resized or moved
            // between displays. Reconfiguring and skipping one frame is the
            // whole recovery.
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("picture"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if let Some(planes) = &self.planes {
                let (x, y, w, h) = letterbox(
                    self.config.width as f32,
                    self.config.height as f32,
                    planes.width as f32,
                    planes.height as f32,
                );
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &planes.bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    fn plane_texture(&self, width: u32, height: u32, data: &[u8]) -> wgpu::Texture {
        self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some("plane"),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &fill(data, (width.max(1) * height.max(1)) as usize),
        )
    }

    fn bind(&self, y: &wgpu::Texture, u: &wgpu::Texture, v: &wgpu::Texture) -> wgpu::BindGroup {
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let (y, u, v) = (view(y), view(u), view(v));
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&u),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&v),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

fn plane_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, width: u32, height: u32, data: &[u8]) {
    let (width, height) = (width.max(1), height.max(1));
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &fill(data, (width * height) as usize),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// Make a plane exactly the length the GPU expects.
///
/// A decoder that hands back a short plane must not become a validation
/// panic; the missing rows are better shown as grey for one frame.
fn fill(data: &[u8], expected: usize) -> std::borrow::Cow<'_, [u8]> {
    if data.len() == expected {
        return std::borrow::Cow::Borrowed(data);
    }
    let mut out = vec![0u8; expected];
    let take = data.len().min(expected);
    out[..take].copy_from_slice(&data[..take]);
    std::borrow::Cow::Owned(out)
}

/// The viewport that keeps a picture's aspect ratio inside a window.
///
/// Shared with the input path in spirit, but computed in physical pixels
/// here; the two must agree or the pointer lands somewhere other than where
/// it is drawn.
fn letterbox(window_w: f32, window_h: f32, picture_w: f32, picture_h: f32) -> (f32, f32, f32, f32) {
    if picture_w <= 0.0 || picture_h <= 0.0 || window_w <= 0.0 || window_h <= 0.0 {
        return (0.0, 0.0, window_w.max(1.0), window_h.max(1.0));
    }
    let scale = (window_w / picture_w).min(window_h / picture_h);
    let w = picture_w * scale;
    let h = picture_h * scale;
    ((window_w - w) / 2.0, (window_h - h) / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_viewport_matches_what_the_input_path_computes() {
        // If these ever disagree the pointer lands somewhere other than where
        // the user sees it, which is the single most confusing failure a
        // remote desktop can have.
        let (x, y, w, h) = letterbox(1000.0, 1000.0, 1920.0, 1080.0);
        let viewport = crate::input::Viewport::fit((1000.0, 1000.0), (1920, 1080));
        assert!((f64::from(x) - viewport.x).abs() < 0.01);
        assert!((f64::from(y) - viewport.y).abs() < 0.01);
        assert!((f64::from(w) - viewport.width).abs() < 0.01);
        assert!((f64::from(h) - viewport.height).abs() < 0.01);
    }

    #[test]
    fn a_degenerate_window_still_produces_a_usable_viewport() {
        let (_, _, w, h) = letterbox(0.0, 0.0, 1920.0, 1080.0);
        assert!(w > 0.0 && h > 0.0);
        let (_, _, w, h) = letterbox(800.0, 600.0, 0.0, 0.0);
        assert!(w > 0.0 && h > 0.0);
    }

    #[test]
    fn planes_are_resized_to_what_the_gpu_expects() {
        assert_eq!(&*fill(&[1, 2, 3], 3), &[1, 2, 3]);
        // Short: padded rather than rejected.
        assert_eq!(&*fill(&[1, 2], 4), &[1, 2, 0, 0]);
        // Long: truncated, since the extra bytes are not part of the picture.
        assert_eq!(&*fill(&[1, 2, 3, 4], 2), &[1, 2]);
    }
}
