use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use winapi::shared::{d3d9::*, d3d9caps::D3DCAPS9, d3d9types::*, dxgi::IDXGIFactory, windef::*};
use winapi::um::{
    d3d11::*,
    unknwnbase::{IUnknown, IUnknownVtbl},
};

use com_impl::{implementation, interface, ComInterface};
use comptr::ComPtr;

use super::shader::VertexDeclaration;
use super::state::{DeviceState, StateBlock};
use super::{Surface, SurfaceData, SwapChain, Texture};

use crate::core::{fmt::d3d_format_to_dxgi, msample::d3d9_to_dxgi_samples, *};
use crate::{Error, Result};

/// Structure representing a logical graphics device.
#[interface(IDirect3DDevice9)]
pub struct Device {
    refs: AtomicU32,
    // Interface which created this device.
    parent: *const Context,
    // The adapter this device represents.
    //
    // Since D3D11 is thread-safe, we allow multiple logical devices
    // to share the same adapter.
    adapter: *const Adapter,
    // The equivalent interface from D3D11.
    device: ComPtr<ID3D11Device>,
    // The context in which commands are run.
    ctx: ComPtr<ID3D11DeviceContext>,
    // Store the creation params, since the app might request them later.
    creation_params: D3DDEVICE_CREATION_PARAMETERS,
    // The DXGI factory which was used to create this device.
    // Required when creating new swap chains.
    factory: ComPtr<IDXGIFactory>,
    // The window associated with this device.
    window: HWND,
    // The implicit swap chain for the back buffer.
    // There is one for each device in an adapter group.
    swap_chains: Vec<ComPtr<SwapChain>>,
    // The device's currently set render targets.
    render_targets: Vec<Option<ComPtr<Surface>>>,
    // The device's current depth / stencil buffer.
    depth_stencil: Option<ComPtr<Surface>>,

    // The current internal state of this device,
    // as it was last set by calling state functions.
    istate: DeviceState,
}

impl Device {
    /// Creates a new device.
    pub fn new(
        parent: &Context,
        adapter: &Adapter,
        cp: D3DDEVICE_CREATION_PARAMETERS,
        pp: &mut D3DPRESENT_PARAMETERS,
        factory: ComPtr<IDXGIFactory>,
    ) -> Result<ComPtr<Device>> {
        let device = adapter.device();
        let ctx = unsafe {
            let mut ptr = ptr::null_mut();
            device.GetImmediateContext(&mut ptr);
            ComPtr::new(ptr)
        };

        // Determine which window to render to.
        // TODO: track the focus window and use it to disable rendering
        // when the app loses focus. It is currently ignored.
        let window = unsafe {
            // We're supposed to use the device window if available, or
            // fall back to the focus window otherwise.
            pp.hDeviceWindow
                .as_mut()
                .or_else(|| cp.hFocusWindow.as_mut())
                .ok_or(Error::InvalidCall)?
        };

        let istate = DeviceState::default();

        let device = Self {
            __vtable: Box::new(Self::create_vtable()),
            refs: AtomicU32::new(1),
            parent,
            adapter,
            device,
            ctx,
            creation_params: cp,
            factory,
            window,
            swap_chains: Vec::new(),
            render_targets: Vec::new(),
            depth_stencil: None,
            istate,
        };

        let mut device: ComPtr<Device> = unsafe { new_com_interface(device) };

        // Create the default swap chain for the adapter.
        device.create_default_swap_chain(pp)?;

        // Create the default render target for the swap chain.
        device.create_default_render_target()?;

        // If the application requested it, we can automatically create
        // a depth/stencil buffer for it.
        if pp.EnableAutoDepthStencil != 0 {
            device.depth_stencil = {
                let width = pp.BackBufferWidth;
                let height = pp.BackBufferHeight;
                let fmt = pp.AutoDepthStencilFormat;
                let discard = pp.Flags & D3DPRESENTFLAG_DISCARD_DEPTHSTENCIL;
                let ms_ty = 0;
                let ms_qlt = 0;
                let shared_handle = 0;

                let mut ptr = ptr::null_mut();

                device.create_depth_stencil_surface(
                    width,
                    height,
                    fmt,
                    ms_ty,
                    ms_qlt,
                    discard,
                    &mut ptr,
                    shared_handle,
                )?;

                Some(ComPtr::new(ptr))
            };
        }

        // Now that we have an initial RT / DS buffer, we need to set D3D11's state.
        device.update_render_targets();

        Ok(device)
    }

    /// Retrieves the adapter of this device.
    pub fn adapter(&self) -> &Adapter {
        unsafe { &*self.adapter }
    }

    /// Retrieves a reference to the immediate device context.
    pub fn device_context(&self) -> &ID3D11DeviceContext {
        &self.ctx
    }

    /// Creates the default swap chain for this device.
    fn create_default_swap_chain(&mut self, pp: &mut D3DPRESENT_PARAMETERS) -> Result<()> {
        // Note: this function is usually used for non-implicit swap chains,
        // but it's a good idea to reuse it.
        let swap_chain = {
            let mut ret = ptr::null_mut();
            self.create_additional_swap_chain(pp, &mut ret)?;
            ComPtr::new(ret)
        };

        // Now put it in the list of implicit swap chains, which should be empty.
        let scs = &mut self.swap_chains;

        assert!(
            scs.is_empty(),
            "Cannot create default swap chain if it already exists"
        );

        scs.push(swap_chain);

        Ok(())
    }

    /// Tries to retrieve a swap chain based on the index.
    fn check_swap_chain(&self, sc: u32) -> Result<&ComPtr<SwapChain>> {
        self.swap_chains.get(sc as usize).ok_or(Error::InvalidCall)
    }

    /// Helper function for creating render targets.
    fn create_render_target_helper(
        &self,
        texture: ComPtr<ID3D11Texture2D>,
    ) -> Result<ComPtr<Surface>> {
        // Create a render target view into the texture.
        let rt_view = unsafe {
            let resource = texture.upcast().as_mut();

            let mut ptr = ptr::null_mut();

            let result = self
                .device
                .CreateRenderTargetView(resource, ptr::null(), &mut ptr);

            check_hresult(result, "Failed to create render target view")?;

            ComPtr::new(ptr)
        };

        let data = SurfaceData::RenderTarget(rt_view);
        let surface = Surface::new(self, texture, 0, data);

        Ok(surface)
    }

    /// Creates the default render target for this device.
    fn create_default_render_target(&mut self) -> Result<()> {
        let sc = &self.swap_chains[0];
        let bbuf = sc.buffer(0)?;

        let rt = self.create_render_target_helper(bbuf)?;

        self.render_targets.push(Some(rt));

        Ok(())
    }

    /// Retrieves a handle to a render target.
    fn check_render_target(&self, i: u32) -> Result<&ComPtr<Surface>> {
        if let Some(rt) = self.render_targets.get(i as usize) {
            if let Some(rt) = rt {
                Ok(rt)
            } else {
                Err(Error::NotFound)
            }
        } else {
            Err(Error::NotFound)
        }
    }

    /// Synchronises D3D9's render target views and depth / stencil view with D3D11.
    fn update_render_targets(&self) {
        let num = self.render_targets.len() as u32;

        let mut rt_views = [ptr::null_mut(); 8];
        for (i, rt) in self.render_targets.iter().enumerate() {
            if let Some(rt) = rt {
                rt_views[i] = rt.render_target_view().unwrap() as *mut _;
            }
        }

        let ds_view = self
            .depth_stencil
            .as_ref()
            .map(|ds| ds.depth_stencil_view().unwrap() as *mut _)
            .unwrap_or(ptr::null_mut());

        unsafe {
            self.ctx.OMSetRenderTargets(num, rt_views.as_ptr(), ds_view);
        }

        // TODO: we also have to set the new viewport.
    }
}

impl_iunknown!(struct Device: IUnknown, IDirect3DDevice9);

#[implementation(IDirect3DDevice9)]
impl Device {
    // -- Device status functions --

    /// Resets the device, recreating all its state.
    fn reset() {
        unimplemented!()
    }

    /// Checks that the device has not yet been lost / reset.
    fn test_cooperative_level() -> Error {
        // Even if the device were lost, we wouldn't be able to do much.
        Error::Success
    }

    /// Determines how much graphics memory is available.
    fn get_available_texture_mem(&self) -> u32 {
        self.adapter().available_memory()
    }

    /// Asks the driver to evict all managed resources from VRAM.
    fn evict_managed_resources() -> Error {
        // Do nothing. The D3D11 driver handles everything.
        Error::Success
    }

    // -- Creation parameters functions --

    /// Returns a reference to the parent interface.
    fn get_direct_3_d(&self, ptr: *mut *mut Context) -> Error {
        let ptr = check_mut_ref(ptr)?;

        *ptr = com_ref(self.parent);

        Error::Success
    }

    /// Returns the caps of this device.
    fn get_device_caps(&self, caps: *mut D3DCAPS9) -> Error {
        let caps = check_mut_ref(caps)?;

        *caps = self.adapter().caps();

        Error::Success
    }

    /// Returns the creation parameters of this device.
    fn get_creation_parameters(&self, params: *mut D3DDEVICE_CREATION_PARAMETERS) -> Error {
        let params = check_mut_ref(params)?;
        *params = self.creation_params;
        Error::Success
    }

    // -- Swap chain functions --

    /// Creates new swap chains for this device.
    fn create_additional_swap_chain(
        &mut self,
        pp: *mut D3DPRESENT_PARAMETERS,
        ret: *mut *mut SwapChain,
    ) -> Error {
        let device = self.device.upcast().as_mut();
        let factory = self.factory.as_mut();
        let pp = check_mut_ref(pp)?;
        let window = self.window;

        let ret = check_mut_ref(ret)?;

        *ret = SwapChain::new(self, device, factory, pp, window)?.into();

        Error::Success
    }

    /// Returns an implicit swap chain.
    fn get_swap_chain(&self, sc: u32, ret: *mut *mut SwapChain) -> Error {
        let sc = self.check_swap_chain(sc)?;
        let ret = check_mut_ref(ret)?;

        *ret = sc.clone().into();

        Error::Success
    }

    /// Returns the number of implicit swap chains.
    fn get_number_of_swap_chains(&self) -> u32 {
        // TODO: to have more than one implicit SC, we need multi-GPU support.
        1
    }

    // The functions below all operate on the implicit swap chains.

    fn present(&self, src: usize, dest: usize, wnd: HWND, dirty: usize) -> Error {
        for sc in &self.swap_chains {
            sc.present(src, dest, wnd, dirty, 0)?;
        }
        Error::Success
    }

    fn get_front_buffer_data(&self, sc: u32, fb: *mut Surface) -> Error {
        self.check_swap_chain(sc)?.get_front_buffer_data(fb)
    }

    fn get_back_buffer(
        &self,
        sc: u32,
        bi: u32,
        ty: D3DBACKBUFFER_TYPE,
        ret: *mut *mut Surface,
    ) -> Error {
        self.check_swap_chain(sc)?.get_back_buffer(bi, ty, ret)
    }

    fn get_raster_status(&self, sc: u32, rs: *mut D3DRASTER_STATUS) -> Error {
        self.check_swap_chain(sc)?.get_raster_status(rs)
    }

    fn get_display_mode(&self, sc: u32, dm: *mut D3DDISPLAYMODE) -> Error {
        self.check_swap_chain(sc)?.get_display_mode(dm)
    }

    // -- Render target functions --

    /// Creates a new render target.
    fn create_render_target(
        &mut self,
        width: u32,
        height: u32,
        fmt: D3DFORMAT,
        ms_ty: D3DMULTISAMPLE_TYPE,
        ms_qlt: u32,
        lockable: u32,
        ret: *mut *mut Surface,
        shared_handle: usize,
    ) -> Error {
        let ret = check_mut_ref(ret)?;

        if lockable != 0 {
            error!("Lockable render targets are not supported");
        }

        if shared_handle != 0 {
            error!("Shared resources are not supported");
            return Error::InvalidCall;
        }

        let device = &self.device;

        // First we need to create a texture we will render to.
        let texture = unsafe {
            let fmt = d3d_format_to_dxgi(fmt);

            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: d3d9_to_dxgi_samples(ms_ty, ms_qlt),
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut ptr = ptr::null_mut();

            let result = device.CreateTexture2D(&desc, ptr::null(), &mut ptr);
            check_hresult(result, "Failed to create 2D texture for render target")?;

            ComPtr::new(ptr)
        };

        *ret = self.create_render_target_helper(texture)?.into();

        Error::Success
    }

    /// Sets a new render target on this device.
    fn set_render_target(&mut self, i: u32, rt: *mut Surface) -> Error {
        if i >= D3D11_SIMULTANEOUS_RENDER_TARGET_COUNT {
            return Error::InvalidCall;
        }

        // The default render target is never allowed to be null.
        if i == 0 && rt.is_null() {
            return Error::InvalidCall;
        }

        let i = i as usize;

        // Ensure the RT vector contains at least as many entries as we need.
        self.render_targets.resize(i + 1, None);

        self.render_targets[i] = if let Some(rt) = unsafe { rt.as_mut() } {
            // Ensure this surface is indeed a render target.
            if rt.render_target_view().is_none() {
                return Error::InvalidCall;
            }

            Some(ComPtr::new(rt))
        } else {
            None
        };

        self.update_render_targets();

        Error::Success
    }

    /// Retrieves a reference to a bound render target.
    fn get_render_target(&self, i: u32, ret: *mut *mut Surface) -> Error {
        let rt = self.check_render_target(i)?;
        let ret = check_mut_ref(ret)?;

        *ret = rt.clone().into();

        Error::Success
    }

    /// Copies a render target's data into a surface.
    fn get_render_target_data(&self, _rt: *mut Surface, _dest: *mut Surface) {
        unimplemented!()
    }

    // -- Depth / stencil buffer functions --

    /// Creates a new depth / stencil buffer.
    fn create_depth_stencil_surface(
        &self,
        width: u32,
        height: u32,
        fmt: D3DFORMAT,
        _ms_ty: D3DMULTISAMPLE_TYPE,
        _ms_qlt: u32,
        discard: u32,
        ret: *mut *mut Surface,
        shared_handle: usize,
    ) -> Error {
        let ret = check_mut_ref(ret)?;

        if shared_handle != 0 {
            error!("Shared resources are not supported");
            return Error::InvalidCall;
        }

        if discard != 0 {
            error!("Discarding depth/stencil buffer not supported");
        }

        let texture = unsafe {
            let fmt = d3d_format_to_dxgi(fmt);

            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                // D/S buffers cannot be multisampled.
                SampleDesc: d3d9_to_dxgi_samples(1, 0),
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_DEPTH_STENCIL,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut ptr = ptr::null_mut();

            let result = self.device.CreateTexture2D(&desc, ptr::null(), &mut ptr);
            check_hresult(result, "Failed to create depth / stencil buffer")?;

            ComPtr::new(ptr)
        };

        let ds_view = unsafe {
            let resource = texture.upcast().as_mut();

            let mut ptr = ptr::null_mut();

            let result = self
                .device
                .CreateDepthStencilView(resource, ptr::null(), &mut ptr);
            check_hresult(result, "Failed to create depth / stencil view")?;

            ComPtr::new(ptr)
        };

        let data = SurfaceData::DepthStencil(ds_view);

        *ret = Surface::new(self, texture, 0, data).into();

        Error::Success
    }

    /// Sets the current depth / stencil buffer.
    fn set_depth_stencil_surface(&mut self, ds: *mut Surface) -> Error {
        self.depth_stencil = if let Some(ds) = unsafe { ds.as_mut() } {
            if ds.depth_stencil_view().is_none() {
                return Error::InvalidCall;
            }

            Some(ComPtr::new(ds))
        } else {
            None
        };

        Error::Success
    }

    /// Retrieves the bound depth / stencil buffer.
    fn get_depth_stencil_surface(&self, ret: *mut *mut Surface) -> Error {
        let ret = check_mut_ref(ret)?;

        *ret = self
            .depth_stencil
            .as_ref()
            .map(|ds| ds.clone().into())
            .unwrap_or(ptr::null_mut());

        Error::Success
    }

    // -- Surface manipulation functions --

    /// Copies a surface's region to
    fn update_surface(
        &self,
        src: *mut Surface,
        sr: *const RECT,
        dest: *mut Surface,
        dp: *const POINT,
    ) -> Error {
        let src = check_mut_ref(src)?;
        let dest = check_mut_ref(dest)?;
        let dp = check_ref(dp)?;

        if src.pool() != D3DPOOL_SYSTEMMEM || dest.pool() != D3DPOOL_DEFAULT {
            return Error::InvalidCall;
        }

        let (src_res, src_subres) = src.subresource();
        let (dest_res, dest_subres) = dest.subresource();

        unsafe {
            let src_box = sr.as_ref().map(|sr| D3D11_BOX {
                left: sr.left as u32,
                top: sr.top as u32,
                front: 0,
                right: sr.right as u32,
                bottom: sr.bottom as u32,
                back: 1,
            });

            let src_box = src_box.map(|b| &b as *const _).unwrap_or(ptr::null());

            self.ctx.CopySubresourceRegion(
                dest_res,
                dest_subres,
                dp.x as u32,
                dp.y as u32,
                0,
                src_res,
                src_subres,
                src_box,
            );
        }

        Error::Success
    }

    fn update_texture() {
        unimplemented!()
    }
    fn stretch_rect() {
        unimplemented!()
    }
    fn color_fill() {
        unimplemented!()
    }

    // -- Texture creation functions --

    /// Creates a new texture from the given parameters.
    fn create_texture(
        &self,
        width: u32,
        height: u32,
        levels: u32,
        usage: u32,
        fmt: D3DFORMAT,
        pool: D3DPOOL,
        ret: *mut *mut Texture,
        shared_handle: usize,
    ) -> Error {
        let ret = check_mut_ref(ret)?;

        if usage != 0 {
            unimplemented!("Texture usage flags not yet implemented: {:b}", usage);
        }

        if shared_handle != 0 {
            error!("Shared resources are not supported");
            return Error::InvalidCall;
        }

        // TODO: we should extract a function from the texture creation code
        // used for render targets and d/s buffers.
        let texture = unsafe {
            let fmt = d3d_format_to_dxgi(fmt);

            let mut bind_flags = D3D11_BIND_SHADER_RESOURCE;
            let mut usage = D3D11_USAGE_DEFAULT;
            let mut cpu_flags = 0;

            match pool {
                // Default resources are placed in VRAM.
                D3DPOOL_DEFAULT => (),
                // Managed resources are placed in VRAM if possible, and are backed by system RAM.
                D3DPOOL_MANAGED => {
                    usage = D3D11_USAGE_DYNAMIC;
                    cpu_flags |= D3D11_CPU_ACCESS_WRITE;
                }
                // SystemMem resources are stored in RAM.
                // Because of this, they are not accessible in shaders.
                D3DPOOL_SYSTEMMEM => {
                    usage = D3D11_USAGE_STAGING;
                    cpu_flags |= D3D11_CPU_ACCESS_WRITE | D3D11_CPU_ACCESS_READ;
                    bind_flags = 0;
                }
                _ => error!("Unsupported memory pool: {}", pool),
            }

            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: levels,
                ArraySize: 1,
                Format: fmt,
                // D3D9 does not have multisampled textures.
                SampleDesc: d3d9_to_dxgi_samples(0, 0),
                Usage: usage,
                BindFlags: bind_flags,
                CPUAccessFlags: cpu_flags,
                MiscFlags: 0,
            };

            let mut ptr = ptr::null_mut();

            let result = self.device.CreateTexture2D(&desc, ptr::null(), &mut ptr);
            check_hresult(result, "Failed to create 2D texture")?;

            ComPtr::new(ptr)
        };

        *ret = Texture::new(self, pool, texture, levels).into();

        Error::Success
    }

    fn create_cube_texture() {
        unimplemented!()
    }
    fn create_offscreen_plain_surface() {
        unimplemented!()
    }
    fn create_volume_texture() {
        unimplemented!()
    }

    // -- Drawing functions --

    fn clear() {
        unimplemented!()
    }

    fn begin_scene() {
        unimplemented!()
    }
    fn end_scene() {
        unimplemented!()
    }

    fn draw_indexed_primitive() {
        unimplemented!()
    }
    fn draw_indexed_primitive_u_p() {
        unimplemented!()
    }
    fn draw_primitive() {
        unimplemented!()
    }
    fn draw_primitive_u_p() {
        unimplemented!()
    }

    // -- State block functions --

    /// Creates a new state block which can capture commands.
    fn create_state_block(&mut self, ty: D3DSTATEBLOCKTYPE, ret: *mut *mut StateBlock) -> Error {
        let ret = check_mut_ref(ret)?;

        *ret = StateBlock::new(self, ty)?.into();

        Error::Success
    }

    /// Begins recording a new state block.
    fn begin_state_block(&mut self) -> Error {
        unimplemented!()
    }

    /// Ends recording a state block, and returns a pointer to it.
    fn end_state_block(&mut self, ret: *mut *mut StateBlock) -> Error {
        let _ret = check_mut_ref(ret)?;
        unimplemented!()
    }

    // -- Hardware cursor functions --

    fn set_cursor_position() {
        unimplemented!()
    }
    fn set_cursor_properties() {
        unimplemented!()
    }
    fn show_cursor() {
        unimplemented!()
    }

    // -- Pipeline state functions --
    // All of these functions are captured by state blocks.

    // -- Render state functions --

    /// Sets the render state.
    fn set_render_state(&mut self, state: D3DRENDERSTATETYPE, value: u32) -> Error {
        self.istate.set_render_state(state, value);
        Error::Success
    }

    /// Retrieves the value of the current render state.
    fn get_render_state(&self, state: D3DRENDERSTATETYPE, ret: *mut u32) -> Error {
        let ret = check_mut_ref(ret)?;

        *ret = self.istate.get_render_state(state);

        Error::Success
    }

    /// Validates the current state of the device, or the state of the
    /// currently recording state block, if any.
    fn validate_device() {
        unimplemented!()
    }

    // -- Vertex shader functions --

    /// Creates a declaration of a vertex shader's input.
    fn create_vertex_declaration(
        &self,
        elems: *const D3DVERTEXELEMENT9,
        ret: *mut *mut VertexDeclaration,
    ) -> Error {
        let ret = check_mut_ref(ret)?;

        *ret = VertexDeclaration::new(self, elems).into();

        Error::Success
    }

    /// Sets the current vertex declaration.
    fn set_vertex_declaration(&mut self, decl: *mut VertexDeclaration) -> Error {
        let decl = check_ref(decl)?;
        self.istate.set_vertex_declaration(decl);
        Error::Success
    }

    /// Gets the current vertex declaration.
    fn get_vertex_declaration(&self, ret: *mut *mut VertexDeclaration) -> Error {
        let ret = check_mut_ref(ret)?;
        *ret = com_ref(self.istate.get_vertex_declaration());
        Error::Success
    }

    fn create_vertex_shader() {
        unimplemented!()
    }
    fn set_vertex_shader() {
        unimplemented!()
    }
    fn get_vertex_shader() {
        unimplemented!()
    }
    fn set_vertex_shader_constant_b() {
        unimplemented!()
    }
    fn get_vertex_shader_constant_b() {
        unimplemented!()
    }
    fn set_vertex_shader_constant_f() {
        unimplemented!()
    }
    fn get_vertex_shader_constant_f() {
        unimplemented!()
    }
    fn set_vertex_shader_constant_i() {
        unimplemented!()
    }
    fn get_vertex_shader_constant_i() {
        unimplemented!()
    }

    fn create_vertex_buffer() {
        unimplemented!()
    }

    fn create_index_buffer() {
        unimplemented!()
    }

    fn set_stream_source() {
        unimplemented!()
    }
    fn get_stream_source() {
        unimplemented!()
    }

    fn set_stream_source_freq() {
        unimplemented!()
    }
    fn get_stream_source_freq() {
        unimplemented!()
    }

    // -- Pixel shader functions --

    fn set_sampler_state() {
        unimplemented!()
    }
    fn get_sampler_state() {
        unimplemented!()
    }

    fn create_pixel_shader() {
        unimplemented!()
    }
    fn set_pixel_shader() {
        unimplemented!()
    }
    fn get_pixel_shader() {
        unimplemented!()
    }
    fn set_pixel_shader_constant_b() {
        unimplemented!()
    }
    fn get_pixel_shader_constant_b() {
        unimplemented!()
    }
    fn set_pixel_shader_constant_f() {
        unimplemented!()
    }
    fn get_pixel_shader_constant_f() {
        unimplemented!()
    }
    fn set_pixel_shader_constant_i() {
        unimplemented!()
    }
    fn get_pixel_shader_constant_i() {
        unimplemented!()
    }

    fn get_texture() {
        unimplemented!()
    }
    fn set_texture() {
        unimplemented!()
    }
    fn get_texture_stage_state() {
        unimplemented!()
    }
    fn set_texture_stage_state() {
        unimplemented!()
    }

    // -- Output Merger state --

    fn set_viewport() {
        unimplemented!()
    }
    fn get_viewport() {
        unimplemented!()
    }

    fn set_scissor_rect() {
        unimplemented!()
    }
    fn get_scissor_rect() {
        unimplemented!()
    }

    // -- Query creation --

    fn create_query() {
        unimplemented!()
    }

    // -- Fixed function pipeline --

    fn delete_patch() {
        unimplemented!()
    }
    fn draw_rect_patch() {
        unimplemented!()
    }
    fn draw_tri_patch() {
        unimplemented!()
    }
    fn get_clip_plane() {
        unimplemented!()
    }
    fn get_clip_status() {
        unimplemented!()
    }
    fn get_current_texture_palette() {
        unimplemented!()
    }
    fn get_f_v_f() {
        unimplemented!()
    }
    fn get_gamma_ramp() {
        unimplemented!()
    }
    fn get_indices() {
        unimplemented!()
    }
    fn get_light() {
        unimplemented!()
    }
    fn get_light_enable() {
        unimplemented!()
    }
    fn get_material() {
        unimplemented!()
    }
    fn get_n_patch_mode() {
        unimplemented!()
    }
    fn get_palette_entries() {
        unimplemented!()
    }
    fn get_software_vertex_processing() {
        unimplemented!()
    }
    fn get_transform() {
        unimplemented!()
    }
    fn light_enable() {
        unimplemented!()
    }
    fn multiply_transform() {
        unimplemented!()
    }
    fn process_vertices() {
        unimplemented!()
    }
    fn set_clip_plane() {
        unimplemented!()
    }
    fn set_clip_status() {
        unimplemented!()
    }
    fn set_current_texture_palette() {
        unimplemented!()
    }
    fn set_dialog_box_mode() {
        unimplemented!()
    }
    fn set_f_v_f() {
        unimplemented!()
    }
    fn set_gamma_ramp() {
        unimplemented!()
    }
    fn set_indices() {
        unimplemented!()
    }
    fn set_light() {
        unimplemented!()
    }
    fn set_material() {
        unimplemented!()
    }
    fn set_n_patch_mode() {
        unimplemented!()
    }
    fn set_palette_entries() {
        unimplemented!()
    }
    fn set_software_vertex_processing() {
        unimplemented!()
    }
    fn set_transform() {
        unimplemented!()
    }
}
