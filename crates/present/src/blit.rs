//! Шейдерный проход композитора: кадр гостя → текстура → back buffer размера окна с
//! масштабированием (билинейная фильтрация) и чёрными полями при несовпадении пропорций.
//!
//! Шейдеры компилируются при запуске `D3DCompile` (`d3dcompiler_47.dll` есть во всех Windows 10/11);
//! если что-то не создалось, композитор работает по-старому — копией кадра без шейдеров.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D_ROOT_SIGNATURE_VERSION_1, D3D12_BLEND_DESC, D3D12_BLEND_ONE, D3D12_BLEND_OP_ADD,
    D3D12_BLEND_ZERO, D3D12_COLOR_WRITE_ENABLE_ALL, D3D12_COMPARISON_FUNC_ALWAYS,
    D3D12_COMPARISON_FUNC_NEVER, D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_CULL_MODE_NONE,
    D3D12_DEPTH_STENCIL_DESC, D3D12_DEPTH_STENCILOP_DESC, D3D12_DESCRIPTOR_HEAP_DESC,
    D3D12_DESCRIPTOR_HEAP_FLAG_NONE, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_HEAP_TYPE_RTV, D3D12_DESCRIPTOR_RANGE,
    D3D12_DESCRIPTOR_RANGE_TYPE_SRV, D3D12_FILL_MODE_SOLID, D3D12_FILTER_MIN_MAG_MIP_LINEAR,
    D3D12_FLOAT32_MAX, D3D12_GRAPHICS_PIPELINE_STATE_DESC, D3D12_HEAP_FLAG_NONE,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_LOGIC_OP_NOOP,
    D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE, D3D12_RASTERIZER_DESC, D3D12_RENDER_TARGET_BLEND_DESC,
    D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_STATE_COPY_DEST,
    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_PRESENT,
    D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_ROOT_DESCRIPTOR_TABLE, D3D12_ROOT_PARAMETER,
    D3D12_ROOT_PARAMETER_0, D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE, D3D12_ROOT_SIGNATURE_DESC,
    D3D12_ROOT_SIGNATURE_FLAG_NONE, D3D12_SHADER_BYTECODE, D3D12_SHADER_VISIBILITY_PIXEL,
    D3D12_STATIC_SAMPLER_DESC, D3D12_STENCIL_OP_KEEP, D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
    D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, D3D12_TEXTURE_LAYOUT_UNKNOWN, D3D12_VIEWPORT,
    D3D12SerializeRootSignature, ID3D12DescriptorHeap, ID3D12Device, ID3D12GraphicsCommandList,
    ID3D12PipelineState, ID3D12Resource, ID3D12RootSignature,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::{PCSTR, s};

use crate::d3d12::{BUFFERS, Ctx, PresentError, Result, borrow, transition};
use crate::layout::Rect;

/// Формат back buffer'ов при шейдерном проходе.
pub(crate) const TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;

/// Полноэкранный треугольник по `SV_VertexID` и выборка кадра; альфа гостя игнорируется.
const SHADER: &str = r"
Texture2D frame_tex : register(t0);
SamplerState frame_smp : register(s0);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs_main(uint id : SV_VertexID) {
    V o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}
float4 ps_main(V i) : SV_Target {
    return float4(frame_tex.Sample(frame_smp, i.uv).rgb, 1.0);
}
";

/// Текстура кадра на GPU.
struct FrameTexture {
    resource: ID3D12Resource,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    /// Ещё ни разу не копировалась (создана в `COPY_DEST`).
    fresh: bool,
}

/// Один вывод кадра.
pub(crate) struct Pass<'a> {
    /// Кадр в upload-буфере.
    pub src: &'a D3D12_TEXTURE_COPY_LOCATION,
    /// Back buffer.
    pub back: &'a ID3D12Resource,
    /// Индекс back buffer'а.
    pub index: usize,
    /// Куда рисовать кадр.
    pub viewport: Rect,
    /// Размер back buffer'а.
    pub target: (u32, u32),
}

/// Состояние шейдерного прохода.
pub(crate) struct Blit {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    srv_heap: ID3D12DescriptorHeap,
    rtv_heap: ID3D12DescriptorHeap,
    rtv_step: usize,
    texture: Option<FrameTexture>,
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: блоб валиден.
    let ptr = unsafe { blob.GetBufferPointer() }.cast::<u8>();
    // SAFETY: блоб валиден.
    let len = unsafe { blob.GetBufferSize() };
    if ptr.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: буфер блоба из `len` байт живёт, пока жив блоб (время жизни среза привязано к нему).
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

fn blob_text(blob: Option<&ID3DBlob>) -> String {
    blob.map(|b| String::from_utf8_lossy(blob_bytes(b)).trim().to_owned())
        .unwrap_or_default()
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: исходник — статическая строка с указанной длиной, имена — статические C-строки,
    // выходные указатели локальные.
    let r = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("rustdroid-blit"),
            None,
            None::<&ID3DInclude>,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &raw mut code,
            Some(&raw mut errors),
        )
    };
    if let Err(cause) = r {
        tracing::warn!(log = %blob_text(errors.as_ref()), "шейдер композитора не скомпилирован");
        return Err(PresentError::Api {
            what: "D3DCompile",
            cause,
        });
    }
    code.ok_or(PresentError::Api {
        what: "D3DCompile",
        cause: windows::core::Error::empty(),
    })
}

fn bytecode(blob: &ID3DBlob) -> D3D12_SHADER_BYTECODE {
    let b = blob_bytes(blob);
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: b.as_ptr().cast(),
        BytecodeLength: b.len(),
    }
}

fn root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let range = D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: 0,
    };
    let param = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                NumDescriptorRanges: 1,
                pDescriptorRanges: &raw const range,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
    };
    let sampler = D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        MaxLOD: D3D12_FLOAT32_MAX,
        ShaderRegister: 0,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        ..Default::default()
    };
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: 1,
        pParameters: &raw const param,
        NumStaticSamplers: 1,
        pStaticSamplers: &raw const sampler,
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: описание и всё, на что оно ссылается, живут до конца вызова.
    let r = unsafe {
        D3D12SerializeRootSignature(
            &raw const desc,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &raw mut blob,
            Some(&raw mut errors),
        )
    };
    if let Err(cause) = r {
        tracing::warn!(log = %blob_text(errors.as_ref()), "корневая сигнатура не сериализована");
        return Err(PresentError::Api {
            what: "D3D12SerializeRootSignature",
            cause,
        });
    }
    let blob = blob.ok_or(PresentError::Api {
        what: "D3D12SerializeRootSignature",
        cause: windows::core::Error::empty(),
    })?;
    // SAFETY: байты сериализованной сигнатуры из блоба выше.
    unsafe { device.CreateRootSignature(0, blob_bytes(&blob)) }.ctx("CreateRootSignature")
}

fn pipeline(device: &ID3D12Device, root: &ID3D12RootSignature) -> Result<ID3D12PipelineState> {
    let vs = compile(s!("vs_main"), s!("vs_5_0"))?;
    let ps = compile(s!("ps_main"), s!("ps_5_0"))?;
    let target_blend = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: false.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_ZERO,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ZERO,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        #[allow(clippy::cast_possible_truncation, reason = "маска RGBA — 4 бита")]
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let keep = D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP_KEEP,
        StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
        StencilPassOp: D3D12_STENCIL_OP_KEEP,
        StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
    };
    let mut rtv_formats = [DXGI_FORMAT::default(); 8];
    rtv_formats[0] = TARGET_FORMAT;
    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        VS: bytecode(&vs),
        PS: bytecode(&ps),
        BlendState: D3D12_BLEND_DESC {
            AlphaToCoverageEnable: false.into(),
            IndependentBlendEnable: false.into(),
            RenderTarget: [target_blend; 8],
        },
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        },
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
            DepthEnable: false.into(),
            DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            StencilEnable: false.into(),
            FrontFace: keep,
            BackFace: keep,
            ..Default::default()
        },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: rtv_formats,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    // SAFETY: описание и байткод шейдеров (блобы `vs`, `ps`) живут до конца вызова.
    let pso = unsafe { device.CreateGraphicsPipelineState(&raw const desc) }
        .ctx("CreateGraphicsPipelineState");
    // Ссылка на сигнатуру, взятая `clone` для описания, возвращается.
    // SAFETY: поле больше не используется.
    drop(unsafe { ManuallyDrop::take(&mut desc.pRootSignature) });
    pso
}

fn heap(device: &ID3D12Device, desc: &D3D12_DESCRIPTOR_HEAP_DESC) -> Result<ID3D12DescriptorHeap> {
    // SAFETY: описание локальное.
    unsafe { device.CreateDescriptorHeap(desc) }.ctx("CreateDescriptorHeap")
}

impl Blit {
    /// Шейдеры, сигнатура, конвейер и кучи дескрипторов.
    pub(crate) fn new(device: &ID3D12Device) -> Result<Self> {
        let root = root_signature(device)?;
        let pso = pipeline(device, &root)?;
        let srv_heap = heap(
            device,
            &D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
        )?;
        #[allow(clippy::cast_possible_truncation, reason = "BUFFERS = 2")]
        let rtv_heap = heap(
            device,
            &D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: BUFFERS as u32,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 0,
            },
        )?;
        // SAFETY: устройство валидно.
        let rtv_step =
            unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) }
                as usize;
        Ok(Self {
            root,
            pso,
            srv_heap,
            rtv_heap,
            rtv_step,
            texture: None,
        })
    }

    /// Нужна ли новая текстура кадра (размер или формат изменились). Перед
    /// [`Self::create_texture`] композитор дожидается GPU: старая текстура может быть в работе.
    pub(crate) fn needs_texture(&self, width: u32, height: u32, format: DXGI_FORMAT) -> bool {
        self.texture
            .as_ref()
            .is_none_or(|t| (t.width, t.height, t.format) != (width, height, format))
    }

    /// Создаёт текстуру кадра и её SRV.
    pub(crate) fn create_texture(
        &mut self,
        device: &ID3D12Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<()> {
        self.texture = None;
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            ..Default::default()
        };
        let mut res: Option<ID3D12Resource> = None;
        // SAFETY: описания локальные, выходной указатель локальный.
        unsafe {
            device.CreateCommittedResource(
                &raw const heap_props,
                D3D12_HEAP_FLAG_NONE,
                &raw const desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &raw mut res,
            )
        }
        .ctx("CreateCommittedResource (текстура кадра)")?;
        let resource = res.ok_or(PresentError::Api {
            what: "CreateCommittedResource (текстура кадра)",
            cause: windows::core::Error::empty(),
        })?;
        // SAFETY: куча валидна.
        let cpu = unsafe { self.srv_heap.GetCPUDescriptorHandleForHeapStart() };
        // SAFETY: ресурс и дескриптор кучи валидны; описание SRV берётся из ресурса.
        unsafe { device.CreateShaderResourceView(&resource, None, cpu) };
        self.texture = Some(FrameTexture {
            resource,
            width,
            height,
            format,
            fresh: true,
        });
        tracing::info!(
            width,
            height,
            "текстура кадра для шейдерного прохода создана"
        );
        Ok(())
    }

    /// Записывает в `list`: копию кадра из `pass.src` в текстуру, очистку back buffer и
    /// отрисовку кадра в прямоугольник `pass.viewport`.
    ///
    /// Все вызовы списка: список в записывающем состоянии, ресурсы и дескрипторы живы, пока он
    /// исполняется (композитор ждёт забор перед удалением ресурсов и сбросом аллокатора).
    pub(crate) fn record(
        &mut self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        pass: &Pass<'_>,
    ) -> Result<()> {
        let tex = self.texture.as_mut().ok_or(PresentError::BadFrame(0, 0))?;
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: структура живёт до конца функции, текстура — дольше.
            pResource: unsafe { borrow(&tex.resource) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        if !tex.fresh {
            let to_copy = [transition(
                &tex.resource,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )];
            // SAFETY: см. описание функции.
            unsafe { list.ResourceBarrier(&to_copy) };
        }
        // SAFETY: см. описание функции; описания копирования живут до конца функции.
        unsafe { list.CopyTextureRegion(&raw const dst, 0, 0, 0, pass.src, None) };
        let to_draw = [
            transition(
                &tex.resource,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            ),
            transition(
                pass.back,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            ),
        ];
        // SAFETY: см. описание функции.
        unsafe { list.ResourceBarrier(&to_draw) };
        tex.fresh = false;

        // SAFETY: куча валидна.
        let rtv_start = unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        let rtv = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: rtv_start.ptr + pass.index % BUFFERS * self.rtv_step,
        };
        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "размер окна много меньше i32::MAX"
        )]
        let scissor = RECT {
            left: 0,
            top: 0,
            right: pass.target.0 as i32,
            bottom: pass.target.1 as i32,
        };
        let vp = D3D12_VIEWPORT {
            TopLeftX: pass.viewport.x,
            TopLeftY: pass.viewport.y,
            Width: pass.viewport.w,
            Height: pass.viewport.h,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: back buffer жив, RTV пишется в слот кучи текущего back buffer'а.
        unsafe { device.CreateRenderTargetView(pass.back, None, rtv) };
        // SAFETY: см. описание функции; дескриптор локальный.
        unsafe { list.OMSetRenderTargets(1, Some(&raw const rtv), false, None) };
        // SAFETY: см. описание функции.
        unsafe { list.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0], None) };
        // SAFETY: см. описание функции.
        unsafe { list.SetGraphicsRootSignature(&self.root) };
        // SAFETY: см. описание функции.
        unsafe { list.SetDescriptorHeaps(&[Some(self.srv_heap.clone())]) };
        // SAFETY: куча валидна.
        let srv = unsafe { self.srv_heap.GetGPUDescriptorHandleForHeapStart() };
        // SAFETY: см. описание функции; в таблице — SRV текстуры кадра.
        unsafe { list.SetGraphicsRootDescriptorTable(0, srv) };
        // SAFETY: см. описание функции.
        unsafe { list.SetPipelineState(&self.pso) };
        // SAFETY: см. описание функции.
        unsafe { list.RSSetViewports(&[vp]) };
        // SAFETY: см. описание функции.
        unsafe { list.RSSetScissorRects(&[scissor]) };
        // SAFETY: см. описание функции.
        unsafe { list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST) };
        if pass.viewport.w > 0.0 && pass.viewport.h > 0.0 {
            // SAFETY: см. описание функции; конвейер, сигнатура и цели заданы выше.
            unsafe { list.DrawInstanced(3, 1, 0, 0) };
        }
        let to_present = [transition(
            pass.back,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_PRESENT,
        )];
        // SAFETY: см. описание функции.
        unsafe { list.ResourceBarrier(&to_present) };
        Ok(())
    }
}
