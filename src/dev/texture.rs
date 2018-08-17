use std::{
    mem, ptr,
    sync::atomic::{AtomicU32, Ordering},
};

use winapi::{
    shared::{d3d9::*, d3d9types::*, windef::RECT, winerror},
    um::{
        d3d11::*,
        unknwnbase::{IUnknown, IUnknownVtbl},
    },
};

use com_impl::{implementation, interface, ComInterface};
use comptr::ComPtr;

use crate::{core::*, d3d11, Error};

use super::{Device, Resource, Surface, SurfaceData};

/// Structure containing an image and its mip sub-levels.
///
/// Closely matches the `ID3D11Texture2D` interface.
/// Also contains a shader resource view for the texture, since in D3D9
/// textures can only be used in shaders, and not for much else.
#[interface(IDirect3DTexture9)]
pub struct Texture {
    resource: Resource,
    refs: AtomicU32,
    texture: d3d11::Texture2D,
    // Number of mip map levels in this texture.
    levels: u32,
}

impl Texture {
    /// Creates a new texture object.
    pub fn new(
        device: *const Device,
        pool: D3DPOOL,
        texture: d3d11::Texture2D,
        levels: u32,
    ) -> ComPtr<Self> {
        let texture = Self {
            __vtable: Box::new(Self::create_vtable()),
            refs: AtomicU32::new(1),
            resource: Resource::new(device, pool, D3DRTYPE_TEXTURE),
            texture,
            levels,
        };

        unsafe { new_com_interface(texture) }
    }

    /// Retrieves the pool in which this texture was allocated.
    pub fn pool(&self) -> D3DPOOL {
        self.resource.pool()
    }
}

impl_iunknown!(struct Texture: IUnknown, IDirect3DResource9, IDirect3DBaseTexture9, IDirect3DTexture9);

impl ComInterface<IDirect3DResource9Vtbl> for Texture {
    fn create_vtable() -> IDirect3DResource9Vtbl {
        let mut vtbl: IDirect3DResource9Vtbl = Resource::create_vtable();
        vtbl.parent = Self::create_vtable();
        vtbl
    }
}

#[implementation(IDirect3DBaseTexture9)]
impl Texture {
    fn set_l_o_d(&mut self, _lod: u32) -> u32 {
        unimplemented!()
    }
    fn get_l_o_d(&self) -> u32 {
        unimplemented!()
    }

    /// Returns the count of mip levels in this texture.
    fn get_level_count(&self) -> u32 {
        self.levels
    }

    fn set_auto_gen_filter_type(&mut self, _filter: D3DTEXTUREFILTERTYPE) -> Error {
        unimplemented!()
    }
    fn get_auto_gen_filter_type(&self) -> D3DTEXTUREFILTERTYPE {
        unimplemented!()
    }
    fn generate_mip_sub_levels(&mut self) {
        unimplemented!()
    }
}

#[implementation(IDirect3DTexture9)]
impl Texture {
    /// Retrieves the description of a certain mip level.
    fn get_level_desc(&self, level: u32, desc: *mut D3DSURFACE_DESC) -> Error {
        let surface = {
            let mut ptr = ptr::null_mut();
            self.get_surface_level(level, &mut ptr)?;
            ComPtr::new(ptr)
        };

        surface.get_desc(desc)
    }

    /// Retrieves a surface representing a mip level of this texture.
    fn get_surface_level(&self, level: u32, ret: *mut *mut Surface) -> Error {
        let ret = check_mut_ref(ret)?;

        if level >= self.get_level_count() {
            return Error::InvalidCall;
        }

        let data = SurfaceData::SubTexture(self as *const _);

        *ret = Surface::new(self.resource.device(), self.texture.clone(), level, data).into();

        Error::Success
    }

    /// Locks a texture and maps its memory.
    pub fn lock_rect(
        &self,
        level: u32,
        ret: *mut D3DLOCKED_RECT,
        // TODO: maybe track dirty regions for efficiency.
        _r: *const RECT,
        flags: u32,
    ) -> Error {
        let ret = check_mut_ref(ret)?;

        let mut ty = 0;

        if flags & D3DLOCK_READONLY != 0 {
            error!("Reading data from a texture might not work");
            ty |= D3D11_MAP_READ;
        } else {
            ty |= match self.pool() {
                D3DPOOL_MANAGED => D3D11_MAP_WRITE_DISCARD,
                D3DPOOL_SYSTEMMEM => D3D11_MAP_WRITE | D3D11_MAP_READ,
                pool => {
                    error!("Cannot lock texture in memory pool {}", pool);
                    return Error::InvalidCall;
                }
            };

            if flags & D3DLOCK_DISCARD != 0 {
                ty |= D3D11_MAP_WRITE_DISCARD;
            }

            if flags & D3DLOCK_NOOVERWRITE != 0 {
                ty |= D3D11_MAP_WRITE_NO_OVERWRITE;
            }
        }

        let mut fl = 0;

        if flags & D3DLOCK_DONOTWAIT != 0 {
            fl |= D3D11_MAP_FLAG_DO_NOT_WAIT;
        }

        // Try to map the subresource.
        let mapped = unsafe {
            let resource = self.texture.as_resource();

            let mut mapped = mem::uninitialized();

            let result = self
                .resource
                .device_context()
                .Map(resource, level, ty, fl, &mut mapped);

            match result {
                0 => Ok(mapped),
                winerror::DXGI_ERROR_WAS_STILL_DRAWING => Err(Error::WasStillDrawing),
                hr => Err(check_hresult(hr, "Failed to map surface")),
            }
        }?;

        // TODO: we need special handling for pitch with DXT texture formats.
        *ret = D3DLOCKED_RECT {
            Pitch: mapped.RowPitch as i32,
            pBits: mapped.pData,
        };

        Error::Success
    }

    /// Unlocks the locked rectangle of memory.
    pub fn unlock_rect(&self, level: u32) -> Error {
        let resource = self.texture.as_resource();

        unsafe {
            self.resource.device_context().Unmap(resource, level);
        }

        Error::Success
    }

    fn add_dirty_rect(&mut self, _r: *const RECT) -> Error {
        unimplemented!()
    }
}
