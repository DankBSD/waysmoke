use raw_window_handle::{HasRawWindowHandle, RawWindowHandle, WaylandHandle};
use smithay_client_toolkit::reexports::client::{
    protocol::{wl_display, wl_surface},
    Proxy,
};

pub struct ToRWH(pub Proxy<wl_surface::WlSurface>, pub Proxy<wl_display::WlDisplay>);

unsafe impl HasRawWindowHandle for ToRWH {
    fn raw_window_handle(&self) -> RawWindowHandle {
        let mut handle = WaylandHandle::empty();
        handle.surface = self.0.c_ptr() as *mut _;
        handle.display = self.1.c_ptr() as *mut _;
        RawWindowHandle::Wayland(handle)
    }
}
