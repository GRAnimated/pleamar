//! The lens: seeing what is behind the glass so it can be bent.
//!
//! Real glass does not show what is behind it as is: near the edge it bends
//! it, like a drop. That needs the pixels behind, and in Wayland only the
//! compositor has them. It is asked for a capture of the screen where the
//! surface is —with the surface on top, which is the only thing it can give—
//! and it gets unmixed: capture = ours + (1 − our alpha) · background, and we
//! know ours because we paint it on a canvas before presenting it. Measured:
//! the unmixed background is off from the real one by less than half a level
//! out of 255 with a 30 % tint, and by less than two with an 85 % edge.
//!
//! With the background already clean it is blurred a little —the frosting of
//! the glass— and the shapes shader reads it displaced according to the bevel.
//! So pleamar paints the lens itself, without the compositor knowing anything
//! about glass.

use crate::platform::Backdrop;

/// How much what is behind gets frosted, in logical pixels.
const FROST: f32 = 8.0;

pub struct Pipelines {
    unmix: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    own: wgpu::RenderPipeline,
    own_over: wgpu::RenderPipeline,
    linear: wgpu::Sampler,
}

impl Pipelines {
    pub fn new(d: &wgpu::Device) -> Pipelines {
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("backdrop"), source: wgpu::ShaderSource::Wgsl(include_str!("backdrop.wgsl").into()) });
        let pipeline = |fs: &str| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: None,
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let linear = d.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Pipelines { unmix: pipeline("unmix"), blur: pipeline("blur"), own: pipeline("own"), own_over: pipeline("own_over"), linear }
    }
}

pub fn texture(d: &wgpu::Device, label: &str, (w, h): (u32, u32), format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> (wgpu::Texture, wgpu::TextureView) {
    let t = d.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    let v = t.create_view(&Default::default());
    (t, v)
}

/// What a surface with glass has: where each frame is painted before
/// presenting it, the last capture, and the unmixed background —sharp and
/// frosted—.
/// How long a new lens keeps capturing back to back before it relies on
/// «only when something changes», and at least how many captures. The compositor
/// fades a layer in when it appears —a few hundred milliseconds in which its real
/// opacity is less than 1—, and what is unmixed from those captures is wrong:
/// by a little under something dark, by everything under something bright.
/// Something that keeps moving replaces it at once; a glass or a shader that
/// stays still from its first frame kept it for ever.
pub const WARM_UP: u32 = 3;
pub const WARM_UP_TIME: std::time::Duration = std::time::Duration::from_millis(1500);

pub struct Lens {
    pub px: (u32, u32),
    /// Two canvases: one gets painted, the other is the one the compositor has.
    canvases: [(wgpu::Texture, wgpu::TextureView); 2],
    next: usize,
    presented: Option<usize>,
    capture: Option<(wgpu::Texture, wgpu::TextureView, (u32, u32))>,
    /// The unmixed background, in two to go back and forth: each time the one
    /// that is not the current one is written, reading the current one where
    /// it cannot be unmixed.
    backgrounds: [(wgpu::Texture, wgpu::TextureView); 2],
    current: usize,
    half: (wgpu::Texture, wgpu::TextureView),
    blurred: (wgpu::Texture, wgpu::TextureView),
    blur_uniforms: [wgpu::Buffer; 2],
    /// A sample of the last capture, and of which box it was: so as not to
    /// repeat the work if nothing has changed.
    sample: Vec<u8>,
    pub bounds: [i32; 4],
    bounds_uniforms: wgpu::Buffer,
    /// A received capture whose background is still to be unmixed: which
    /// canvas was presented and with which clip.
    pending: Option<(usize, (u32, u32, u32, u32))>,
    /// There is already a background to show.
    pub ready: bool,
    /// How many captures it has taken in, and since when it exists: see `WARM_UP`.
    pub received: u32,
    born: std::time::Instant,
    /// What the shapes shader reads: the sharp background and the frosted one.
    pub group: Option<wgpu::BindGroup>,
}

impl Lens {
    pub fn new(d: &wgpu::Device, format: wgpu::TextureFormat, px: (u32, u32)) -> Lens {
        use wgpu::TextureUsages as U;
        let canvas = || texture(d, "canvas", px, format, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC);
        let background = || texture(d, "background", px, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC | U::COPY_DST);
        let half = (px.0.div_ceil(2), px.1.div_ceil(2));
        let uniforms = || d.create_buffer(&wgpu::BufferDescriptor { label: Some("blur"), size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Lens {
            px,
            canvases: [canvas(), canvas()],
            next: 0,
            presented: None,
            capture: None,
            backgrounds: [background(), background()],
            current: 0,
            half: texture(d, "half", half, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING),
            blurred: texture(d, "blurred", half, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING),
            blur_uniforms: [uniforms(), uniforms()],
            sample: Vec::new(),
            pending: None,
            bounds: [0; 4],
            bounds_uniforms: d.create_buffer(&wgpu::BufferDescriptor { label: Some("bounds"), size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
            ready: false,
            received: 0,
            born: std::time::Instant::now(),
            group: None,
        }
    }

    /// Which of the two canvases this frame is painted on.
    pub fn next(&self) -> usize {
        self.next
    }

    /// Where this frame is painted.
    pub fn canvas(&self) -> &wgpu::TextureView {
        &self.canvases[self.next].1
    }

    /// What was painted goes to the screen: it is copied, and stays as the presented one.
    pub fn copy_to(&mut self, encoder: &mut wgpu::CommandEncoder, screen: &wgpu::Texture) {
        let (w, h) = self.px;
        encoder.copy_texture_to_texture(
            self.canvases[self.next].0.as_image_copy(),
            screen.as_image_copy(),
            wgpu::Extent3d { width: w.min(screen.width()), height: h.min(screen.height()), depth_or_array_layers: 1 },
        );
        self.presented = Some(self.next);
        self.next ^= 1;
    }

    /// A capture of the box `bounds` arrives (logical pixels of the surface):
    /// the background is unmixed and frosted, only there. `true` if something
    /// visible has changed, and it has to be painted again.
    pub fn receive(&mut self, g: &crate::gpu::Gpu, d: Backdrop, scale: f32, bounds: [i32; 4]) -> bool {
        let Some(presented) = self.presented else { return false };
        // A capture without data is one that failed.
        let Some(data) = d.data else { return false };
        let data: &[u8] = (*data).as_ref();
        // Counted even if it brings nothing new: a still background must not
        // keep the warm-up going for ever. During the warm-up none is skipped
        // for looking like the last one: the first ones are the ones not to
        // trust, and with a still scene the next ones look exactly like them.
        self.received += 1;
        // The same as the previous one —or almost: the rounding of going back
        // and forth gives the odd stray level—, and there is nothing new
        // behind. Without this, painting the lens changes the capture, which
        // changes the lens, and so on without end. One byte in thirteen is
        // looked at, which is enough to know whether something has moved.
        let sample: Vec<u8> = data.iter().copied().step_by(13).collect();
        if self.ready && !self.warming() && bounds == self.bounds && sample.len() == self.sample.len() && sample.iter().zip(&self.sample).all(|(a, b)| a.abs_diff(*b) <= 3) {
            return false;
        }
        self.sample = sample;
        self.bounds = bounds;
        let dev = &g.device;
        let size = (d.width, d.height);
        if self.capture.as_ref().is_none_or(|f| f.2 != size) {
            let (tx, v) = texture(dev, "capture", size, wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
            self.capture = Some((tx, v, size));
        }
        let capture = self.capture.as_ref().unwrap();
        g.queue.write_texture(
            capture.0.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(d.stride), rows_per_image: Some(d.height) },
            wgpu::Extent3d { width: d.width, height: d.height, depth_or_array_layers: 1 },
        );
        // The box in real pixels, inside the canvas.
        let (lw, lh) = self.px;
        let x0 = ((bounds[0] as f32 * scale).floor() as u32).min(lw);
        let y0 = ((bounds[1] as f32 * scale).floor() as u32).min(lh);
        let x1 = (((bounds[0] + bounds[2]) as f32 * scale).ceil() as u32).min(lw);
        let y1 = (((bounds[1] + bounds[3]) as f32 * scale).ceil() as u32).min(lh);
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        let scissor = (x0, y0, x1 - x0, y1 - y0);
        g.queue.write_buffer(&self.bounds_uniforms, 0, bytemuck::cast_slice(&[bounds[0] as f32 * scale, bounds[1] as f32 * scale, bounds[2] as f32 * scale, bounds[3] as f32 * scale, d.opacity, 0.0, 0.0, 0.0]));
        // The passes go in the same submission as the frame being painted now
        // —it has to be painted again—: a separate one cost one more `submit`,
        // a third of a millisecond per capture.
        self.pending = Some((presented, scissor));
        let new = self.current ^ 1;
        self.group = Some(g.backdrop_group(&self.backgrounds[new].1, &self.blurred.1));
        self.ready = true;
        true
    }

    /// The unmixed background as it stands, once there is one.
    pub fn background(&self) -> Option<&wgpu::TextureView> {
        self.ready.then(|| &self.backgrounds[self.current].1)
    }

    /// Still in its first moments: see `WARM_UP`.
    pub fn warming(&self) -> bool {
        self.received < WARM_UP || self.born.elapsed() < WARM_UP_TIME
    }

    /// Unmixes and frosts what the last capture left, in the frame's
    /// submission, before painting it.
    pub fn prepare(&mut self, g: &crate::gpu::Gpu, t: &Pipelines, enc: &mut wgpu::CommandEncoder, scale: f32) {
        let Some((presented, scissor)) = self.pending.take() else { return };
        let (Some(capture), (lw, lh)) = (self.capture.as_ref(), self.px) else { return };
        let dev = &g.device;
        // What is outside the box stays as it was: the new background starts
        // out as a copy of the current one, and only the inside is written.
        let new = self.current ^ 1;
        enc.copy_texture_to_texture(self.backgrounds[self.current].0.as_image_copy(), self.backgrounds[new].0.as_image_copy(), wgpu::Extent3d { width: lw, height: lh, depth_or_array_layers: 1 });
        let unmix_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &t.unmix.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&capture.1) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.canvases[presented].1) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&self.backgrounds[self.current].1) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&t.linear) },
                wgpu::BindGroupEntry { binding: 4, resource: self.bounds_uniforms.as_entire_binding() },
            ],
        });
        pass(enc, &t.unmix, &self.backgrounds[new].1, &unmix_group, scissor);
        frost(g, t, enc, &self.backgrounds[new].1, &self.half, &self.blurred, &self.blur_uniforms, scissor, scale);
        self.current = new;
    }
}

/// One full-screen triangle with `pipeline`, clipped to `scissor`, onto what is there.
fn pass(enc: &mut wgpu::CommandEncoder, pipeline: &wgpu::RenderPipeline, target: &wgpu::TextureView, group: &wgpu::BindGroup, scissor: (u32, u32, u32, u32)) {
    let mut p = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    p.set_pipeline(pipeline);
    p.set_bind_group(0, group, &[]);
    p.set_scissor_rect(scissor.0, scissor.1, scissor.2, scissor.3);
    p.draw(0..3, 0..1);
}

/// Frosts the sharp background in `scissor` (real pixels), at half
/// resolution: horizontal into `half`, vertical into `blurred`.
#[allow(clippy::too_many_arguments)]
fn frost(g: &crate::gpu::Gpu, t: &Pipelines, enc: &mut wgpu::CommandEncoder, sharp: &wgpu::TextureView, half: &(wgpu::Texture, wgpu::TextureView), blurred: &(wgpu::Texture, wgpu::TextureView), uniforms: &[wgpu::Buffer; 2], scissor: (u32, u32, u32, u32), scale: f32) {
    let dev = &g.device;
    let (mw, mh) = (half.0.width(), half.0.height());
    let sigma = FROST * scale * 0.5;
    for (k, step) in [[1.0f32, 0.0], [0.0, 1.0]].iter().enumerate() {
        g.queue.write_buffer(&uniforms[k], 0, bytemuck::cast_slice(&[step[0], step[1], sigma, 0.0, mw as f32, mh as f32, 0.0, 0.0]));
    }
    let blur = |source: &wgpu::TextureView, u: &wgpu::Buffer| {
        dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &t.blur.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) },
                wgpu::BindGroupEntry { binding: 1, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&t.linear) },
            ],
        })
    };
    // The clip, halved, and with the margin the gaussian reaches.
    let m = (sigma * 2.5).ceil() as u32 + 1;
    let x0 = (scissor.0 / 2).saturating_sub(m);
    let y0 = (scissor.1 / 2).saturating_sub(m);
    let x1 = ((scissor.0 + scissor.2).div_ceil(2) + m).min(mw);
    let y1 = ((scissor.1 + scissor.3).div_ceil(2) + m).min(mh);
    let half_clip = (x0, y0, x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
    pass(enc, &t.blur, &half.1, &blur(sharp, &uniforms[0]), half_clip);
    pass(enc, &t.blur, &blurred.1, &blur(&half.1, &uniforms[1]), half_clip);
}

/// The lens of a scene that is the whole screen —pleamar-wm's—: there is no
/// compositor to ask what is behind, because what is behind is what the scene
/// painted itself before the glass (the wallpaper, the windows). Right before
/// a glass is painted, that is copied and frosted, and the glass bends it like
/// any other background.
///
/// A scene on a surface uses it too, when a glass of its has something of the
/// scene's own under it: then what is behind that glass is what the scene
/// painted before it, over whatever the capture says is behind the surface.
pub struct Own {
    pub px: (u32, u32),
    sharp: (wgpu::Texture, wgpu::TextureView),
    half: (wgpu::Texture, wgpu::TextureView),
    blurred: (wgpu::Texture, wgpu::TextureView),
    blur_uniforms: [wgpu::Buffer; 2],
    /// What the shapes shader reads.
    pub group: wgpu::BindGroup,
}

impl Own {
    pub fn new(g: &crate::gpu::Gpu, px: (u32, u32)) -> Own {
        use wgpu::TextureUsages as U;
        let d = &g.device;
        let half = (px.0.div_ceil(2), px.1.div_ceil(2));
        let uniforms = || d.create_buffer(&wgpu::BufferDescriptor { label: Some("blur"), size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let sharp = texture(d, "painted below the glass", px, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING);
        let blurred = texture(d, "blurred", half, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING);
        let group = g.backdrop_group(&sharp.1, &blurred.1);
        Own { px, sharp, half: texture(d, "half", half, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING), blurred, blur_uniforms: [uniforms(), uniforms()], group }
    }

    /// What has been painted so far in `painted`, in `scissor` (real pixels),
    /// becomes what is behind the glass; with `behind` —the unmixed background
    /// of a surface—, over that.
    pub fn take(&self, g: &crate::gpu::Gpu, t: &Pipelines, enc: &mut wgpu::CommandEncoder, painted: &wgpu::TextureView, behind: Option<&wgpu::TextureView>, scissor: (u32, u32, u32, u32), scale: f32) {
        let pipeline = if behind.is_some() { &t.own_over } else { &t.own };
        let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(painted) }];
        if let Some(behind) = behind {
            entries.push(wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(behind) });
        }
        let group = g.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipeline.get_bind_group_layout(0), entries: &entries });
        pass(enc, pipeline, &self.sharp.1, &group, scissor);
        frost(g, t, enc, &self.sharp.1, &self.half, &self.blurred, &self.blur_uniforms, scissor, scale);
    }
}

/// At most, one capture of what is behind every so often while painting.
pub const CAPTURE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How much margin the frosting needs around a glass, in logical pixels.
pub const MARGIN: f32 = FROST * 2.5 + 2.0;
