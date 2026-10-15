//! GPU textures for remote desktops. Each session owns one texture; per displayed frame only the
//! rectangles that changed are uploaded, straight from the shared frame buffer.

use std::collections::HashMap;
use std::sync::Arc;

use egui_wgpu::{RenderState, wgpu};

use crate::session::{SessionId, SharedFrame};

struct RemoteTexture {
    gpu_source: Option<Arc<crate::gpu::GpuImage>>,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    id: egui::TextureId,
    width: u32,
    height: u32,
    filter: wgpu::FilterMode,
}

#[derive(Debug, Clone, Copy)]
pub struct TextureInfo {
    pub id: egui::TextureId,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
pub struct RemoteTextures {
    textures: HashMap<SessionId, RemoteTexture>,
}

impl RemoteTextures {
    /// Uploads pending changes for every session and drops textures of closed sessions.
    /// `filters` says, per session, whether the image is shown scaled (linear) or 1:1 (nearest).
    pub fn sync(
        &mut self,
        render_state: &RenderState,
        sessions: &[(SessionId, Arc<SharedFrame>)],
        filters: &HashMap<SessionId, wgpu::FilterMode>,
    ) {
        self.textures.retain(|id, texture| {
            let alive = sessions.iter().any(|(sid, _)| sid == id);
            if !alive {
                render_state.renderer.write().free_texture(&texture.id);
            }
            alive
        });

        for (id, frame) in sessions {
            frame.clear_wake();
            let mut data = frame.lock();
            let (width, height) = (data.width(), data.height());
            if width == 0 || height == 0 {
                continue;
            }

            let filter = filters
                .get(id)
                .copied()
                .unwrap_or(wgpu::FilterMode::Nearest);
            let needs_new = self.textures.get(id).is_none_or(|t| {
                t.width != width
                    || t.height != height
                    || match (&t.gpu_source, &data.gpu) {
                        (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                        (None, None) => false,
                        _ => true,
                    }
            });

            if needs_new {
                if let Some(old) = self.textures.remove(id) {
                    render_state.renderer.write().free_texture(&old.id);
                }
                let texture = if let Some(image) = &data.gpu {
                    let view = image.texture.create_view(&Default::default());
                    let id = render_state.renderer.write().register_native_texture(
                        &render_state.device,
                        &view,
                        filter,
                    );
                    RemoteTexture {
                        gpu_source: Some(Arc::clone(image)),
                        texture: image.texture.clone(),
                        view,
                        id,
                        width,
                        height,
                        filter,
                    }
                } else {
                    create_texture(render_state, width, height, filter)
                };
                self.textures.insert(*id, texture);
                data.mark_all_dirty();
            }

            let texture = self.textures.get_mut(id).expect("just ensured");
            if texture.filter != filter {
                render_state
                    .renderer
                    .write()
                    .update_egui_texture_from_wgpu_texture(
                        &render_state.device,
                        &texture.view,
                        filter,
                        texture.id,
                    );
                texture.filter = filter;
            }

            if data.gpu.is_some() {
                continue;
            }

            let stride = data.stride();
            for rect in data.take_dirty() {
                let start = rect.y as usize * stride + rect.x as usize * 4;
                render_state.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: rect.x,
                            y: rect.y,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &data.data()[start..],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride as u32),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: rect.w,
                        height: rect.h,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
    }

    pub fn get(&self, id: SessionId) -> Option<TextureInfo> {
        self.textures.get(&id).map(|t| TextureInfo {
            id: t.id,
            width: t.width,
            height: t.height,
        })
    }
}

fn create_texture(
    render_state: &RenderState,
    width: u32,
    height: u32,
    filter: wgpu::FilterMode,
) -> RemoteTexture {
    let texture = render_state
        .device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("remote desktop"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // egui works in gamma space and expects non-sRGB native textures.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let id =
        render_state
            .renderer
            .write()
            .register_native_texture(&render_state.device, &view, filter);
    RemoteTexture {
        gpu_source: None,
        texture,
        view,
        id,
        width,
        height,
        filter,
    }
}
