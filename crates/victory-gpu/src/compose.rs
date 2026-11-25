//! Display planes put together for the window. A console title can present
//! several planes at once (a game image and a UI drawn over it, each at its
//! own size); the console's display hardware scales and blends them. Ours
//! is a compute shader of ours (HLSL compiled by the system's compiler, a
//! 5.1 shader with its root signature inside) that samples each plane's
//! source rectangle into its place on the screen and blends it over the
//! planes before it.
//!
//! shortcut: planes past the first blend as premultiplied alpha, and the
//! title's plane flags and color spaces are not read; at most `PLANES`
//! planes are put together.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, transition};

/// Planes put together at most.
pub(crate) const PLANES: usize = 4;

const SOURCE: &str = r#"
#define RS "DescriptorTable(UAV(u0), SRV(t0, numDescriptors=4)), RootConstants(num32BitConstants=36, b0), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP)"
RWTexture2D<float4> output : register(u0);
Texture2D<float4> planes[4] : register(t0);
SamplerState clamped : register(s0);
// Per plane: source and screen rectangles (left, top, right, bottom), both
// as fractions of the plane and of the screen.
cbuffer Planes : register(b0) { uint count; uint width; uint height; uint unused; float4 source[4]; float4 screen[4]; };
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= width || id.y >= height) return;
    float2 at = (float2(id.xy) + 0.5) / float2(width, height);
    float3 color = 0;
    for (uint i = 0; i < count; i++) {
        float4 d = screen[i];
        if (any(at < d.xy) || any(at >= d.zw)) continue;
        float2 uv = lerp(source[i].xy, source[i].zw, (at - d.xy) / (d.zw - d.xy));
        float4 c = planes[i].SampleLevel(clamped, uv, 0);
        color = i == 0 ? c.rgb : c.rgb + color * (1 - c.a);
    }
    output[id.xy] = float4(color, 1);
}
"#;

/// One plane: its texture, the state it is in, and its source and screen
/// rectangles as fractions (left, top, right, bottom).
pub(crate) struct Layer<'a> {
    pub(crate) texture: &'a ID3D12Resource,
    pub(crate) state: u32,
    pub(crate) source: [f32; 4],
    pub(crate) screen: [f32; 4],
}

/// The shader, its views and the texture it writes, kept between frames.
pub(crate) struct Composer {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    heap: ID3D12DescriptorHeap,
    /// The image put together, in the unordered access state between uses,
    /// with its size and format.
    output: Option<(ID3D12Resource, u32, u32, DXGI_FORMAT)>,
}

impl Composer {
    pub(crate) fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; out pointers ours.
        let compiled = unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("compose"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = compiled {
            if let Some(errors) = errors {
                // SAFETY: the blob's own bytes.
                let text = unsafe {
                    std::slice::from_raw_parts(
                        errors.GetBufferPointer().cast::<u8>(),
                        errors.GetBufferSize(),
                    )
                };
                tracing::warn!("plane composer: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        // SAFETY: the blob's own bytes; the root signature is inside the shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
            let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            let heap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1 + PLANES as u32,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            Ok(Self {
                root,
                pso,
                heap,
                output: None,
            })
        }
    }

    /// The texture `width` by `height` in `format` the planes are put
    /// together in (made again when either changes), in the unordered
    /// access state.
    pub(crate) fn output(
        &mut self,
        device: &ID3D12Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<&ID3D12Resource> {
        if !matches!(&self.output, Some((_, w, h, f)) if (*w, *h, *f) == (width, height, format)) {
            self.output = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: width.into(),
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut texture: Option<ID3D12Resource> = None;
            // SAFETY: plain creation of a texture of ours.
            unsafe {
                device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    None,
                    &mut texture,
                )?;
            }
            let texture = texture.ok_or_else(windows::core::Error::empty)?;
            self.output = Some((texture, width, height, format));
        }
        Ok(&self.output.as_ref().expect("made above").0)
    }

    /// Records, in `list`, the planes put together in the output texture
    /// (made by [`Composer::output`] first), which is left in the
    /// unordered access state. Each plane goes back to its own state.
    pub(crate) fn record(&self, device: &ID3D12Device, list: &HostList, layers: &[Layer]) {
        let Some((output, width, height, format)) = &self.output else {
            return;
        };
        let layers = &layers[..layers.len().min(PLANES)];
        let mut constants = [0u32; 36];
        constants[..3].copy_from_slice(&[layers.len() as u32, *width, *height]);
        // SAFETY: plain view creation in our heap; an open list; our root
        // signature, pipeline and live textures the caller holds.
        unsafe {
            let start = self.heap.GetCPUDescriptorHandleForHeapStart();
            let step = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let slot = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + i * step,
            };
            let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: *format,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            device.CreateUnorderedAccessView(output, None, Some(&uav), slot(0));
            for i in 0..PLANES {
                let (texture, view_format) = match layers.get(i) {
                    Some(l) => {
                        let f = crate::host::swap_format(l.texture.GetDesc().Format)
                            .unwrap_or(DXGI_FORMAT_R8G8B8A8_UNORM);
                        (Some(l.texture), f)
                    }
                    None => (None, DXGI_FORMAT_R8G8B8A8_UNORM),
                };
                let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: view_format,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MipLevels: 1,
                            ..Default::default()
                        },
                    },
                };
                device.CreateShaderResourceView(texture, Some(&srv), slot(1 + i));
            }
            for (i, l) in layers.iter().enumerate() {
                constants[4 + i * 4..8 + i * 4].copy_from_slice(&l.source.map(f32::to_bits));
                constants[20 + i * 4..24 + i * 4].copy_from_slice(&l.screen.map(f32::to_bits));
            }
            const READ: u32 = 0x40;
            let reads = |l: &Layer| l.state & READ == 0;
            let into: Vec<_> = layers
                .iter()
                .filter(|l| reads(l))
                .map(|l| {
                    transition(
                        l.texture,
                        D3D12_RESOURCE_STATES(l.state as i32),
                        D3D12_RESOURCE_STATES(READ as i32),
                    )
                })
                .collect();
            let l = &list.list;
            if !into.is_empty() {
                l.ResourceBarrier(&into);
            }
            l.SetDescriptorHeaps(&[Some(self.heap.clone())]);
            l.SetComputeRootSignature(&self.root);
            l.SetPipelineState(&self.pso);
            l.SetComputeRootDescriptorTable(0, self.heap.GetGPUDescriptorHandleForHeapStart());
            l.SetComputeRoot32BitConstants(1, constants.len() as u32, constants.as_ptr().cast(), 0);
            l.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
            let back: Vec<_> = layers
                .iter()
                .filter(|l| reads(l))
                .map(|l| {
                    transition(
                        l.texture,
                        D3D12_RESOURCE_STATES(READ as i32),
                        D3D12_RESOURCE_STATES(l.state as i32),
                    )
                })
                .collect();
            if !back.is_empty() {
                l.ResourceBarrier(&back);
            }
        }
    }
}
