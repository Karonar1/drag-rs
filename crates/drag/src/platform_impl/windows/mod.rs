// Copyright 2023-2023 CrabNebula Ltd.
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{CursorPosition, DragItem, DragMode, DragResult, Image, Options};

use std::{
    ffi::c_void,
    iter::once,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::addr_of,
    sync::Once,
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::{GetObjectW, BITMAP},
        System::{
            Com::*,
            DataExchange::RegisterClipboardFormatW,
            Memory::*,
            Ole::{
                DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, CF_HDROP, DROPEFFECT,
                DROPEFFECT_COPY, DROPEFFECT_MOVE,
            },
            SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
        },
        UI::{
            Shell::{
                BHID_DataObject, CLSID_DragDropHelper, Common, IDragSourceHelper, IShellItemArray,
                SHCreateDataObject, SHCreateShellItemArrayFromIDLists, SHCreateStdEnumFmtEtc,
                CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, DROPFILES, FD_UNICODE, FILEDESCRIPTORW,
                FILEGROUPDESCRIPTORW, SHDRAGIMAGE,
            },
            WindowsAndMessaging::GetCursorPos,
        },
    },
};

mod image;

static mut OLE_RESULT: Result<()> = Ok(());
static OLE_UNINITIALIZE: Once = Once::new();
fn init_ole() {
    OLE_UNINITIALIZE.call_once(|| {
        unsafe {
            OLE_RESULT = OleInitialize(Some(std::ptr::null_mut()));
        }
        // I guess we never deinitialize for now?
        // OleUninitialize
    });
}

#[implement(IDataObject)]
struct DataObject {
    item: DragItem,
    inner_shell_obj: IDataObject,
    content_format: u16,
    descriptor_format: u16,
}

#[implement(IDropSource)]
struct DropSource(());

impl DropSource {
    fn new() -> Self {
        Self(())
    }
}

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSource {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (grfkeystate & MK_LBUTTON) == MODIFIERKEYS_FLAGS(0) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

impl DataObject {
    // This will be used for sharing text between applications
    #[allow(dead_code)]
    fn new(item: DragItem) -> Self {
        unsafe {
            Self {
                item,
                inner_shell_obj: SHCreateDataObject(None, None, None).unwrap(),
                content_format: RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
                descriptor_format: RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16,
            }
        }
    }

    fn is_global_content(pformatetc: *const FORMATETC) -> bool {
        if let Some(format_etc) = unsafe { pformatetc.as_ref() } {
            // API documentation all suggests tymed should be exact, but in reality Explorer uses
            // it as a bitfield when requesting data.
            (format_etc.tymed as i32 & TYMED_HGLOBAL.0) > 0
                && format_etc.dwAspect == DVASPECT_CONTENT.0
        } else {
            false
        }
    }

    fn hdrop_data(files: &[PathBuf]) -> Result<HGLOBAL> {
        let mut buffer = Vec::new();
        for path in files {
            let wide_path: Vec<u16> = std::path::absolute(path)
                .unwrap()
                .as_os_str()
                .encode_wide()
                .chain(once(0))
                .collect();
            buffer.extend(wide_path);
        }
        buffer.push(0);
        let size = std::mem::size_of::<DROPFILES>() + buffer.len() * 2;
        let handle = get_hglobal(size, buffer)?;
        Ok(handle)
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for DataObject {
    fn GetData(&self, pformatetc: *const FORMATETC) -> Result<STGMEDIUM> {
        if let Some(format) = unsafe { pformatetc.as_ref() } {
            if Self::is_global_content(pformatetc) {
                match &self.item {
                    DragItem::Files(path_bufs) => {
                        if format.cfFormat == CF_HDROP.0 {
                            return Ok(STGMEDIUM {
                                tymed: TYMED_HGLOBAL.0 as u32,
                                u: STGMEDIUM_0 {
                                    hGlobal: Self::hdrop_data(path_bufs)?,
                                },
                                pUnkForRelease: std::mem::ManuallyDrop::new(None),
                            });
                        }
                    }
                    DragItem::Data { provider, types } => {
                        if format.cfFormat == self.content_format {
                            if let Some(data) = provider(&types[format.lindex as usize]) {
                                unsafe {
                                    let handle = GlobalAlloc(GMEM_FIXED, data.len()).unwrap();
                                    let ptr = GlobalLock(handle);
                                    std::ptr::copy(data.as_ptr() as *const c_void, ptr, data.len());
                                    GlobalUnlock(handle).unwrap();
                                    return Ok(STGMEDIUM {
                                        tymed: TYMED_HGLOBAL.0 as u32,
                                        u: STGMEDIUM_0 { hGlobal: handle },
                                        pUnkForRelease: std::mem::ManuallyDrop::new(None),
                                    });
                                }
                            }
                        } else if format.cfFormat == self.descriptor_format {
                            let size = size_of::<FILEGROUPDESCRIPTORW>()
                                + size_of::<FILEDESCRIPTORW>() * (types.len() - 1);
                            unsafe {
                                let handle = GlobalAlloc(GMEM_FIXED, size).unwrap();
                                let ptr = GlobalLock(handle);

                                let group_descriptor = ptr as *mut FILEGROUPDESCRIPTORW;
                                (*group_descriptor).cItems = types.len() as u32;

                                let mut descriptor = (*group_descriptor).fgd.as_mut_ptr();

                                for path in types {
                                    (*descriptor).dwFlags = FD_UNICODE.0 as u32;

                                    let mut buffer = Vec::new();
                                    let wide_path: Vec<u16> = std::path::absolute(path)
                                        .unwrap()
                                        .as_os_str()
                                        .encode_wide()
                                        .chain(once(0))
                                        .collect();
                                    buffer.extend(wide_path);
                                    buffer.push(0);

                                    assert!(buffer.len() <= 260);
                                    std::ptr::copy(
                                        buffer.as_ptr(),
                                        addr_of!((*descriptor).cFileName) as *mut u16,
                                        buffer.len(),
                                    );

                                    descriptor = descriptor.add(1);
                                }
                                GlobalUnlock(handle).unwrap();

                                return Ok(STGMEDIUM {
                                    tymed: TYMED_HGLOBAL.0 as u32,
                                    u: STGMEDIUM_0 { hGlobal: handle },
                                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                                });
                            }
                        }
                    }
                }
            }
        }
        unsafe { self.inner_shell_obj.GetData(pformatetc) }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
        Err(Error::new(DV_E_FORMATETC, HSTRING::new()))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if Self::is_global_content(pformatetc) {
            if let Some(format) = unsafe { pformatetc.as_ref() } {
                match self.item {
                    DragItem::Files(_) => {
                        if format.cfFormat == CF_HDROP.0 {
                            return S_OK;
                        }
                    }
                    DragItem::Data { .. } => {
                        if format.cfFormat == self.content_format
                            || format.cfFormat == self.descriptor_format
                        {
                            return S_OK;
                        }
                    }
                }
            }
        }
        unsafe { self.inner_shell_obj.QueryGetData(pformatetc) }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
        E_NOTIMPL
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> Result<()> {
        unsafe { self.inner_shell_obj.SetData(pformatetc, pmedium, frelease) }
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> Result<IEnumFORMATETC> {
        match &self.item {
            DragItem::Files(_) => unsafe {
                SHCreateStdEnumFmtEtc(&[FORMATETC {
                    cfFormat: CF_HDROP.0,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: 0,
                    tymed: TYMED_HGLOBAL.0 as u32,
                }])
            },
            DragItem::Data { .. } => unsafe {
                SHCreateStdEnumFmtEtc(&[
                    FORMATETC {
                        cfFormat: self.content_format,
                        ptd: std::ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT.0,
                        lindex: 0,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    },
                    FORMATETC {
                        cfFormat: self.descriptor_format,
                        ptd: std::ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT.0,
                        lindex: 0,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    },
                ])
            },
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Option<&IAdviseSink>,
    ) -> Result<u32> {
        Err(Error::new(OLE_E_ADVISENOTSUPPORTED, HSTRING::new()))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(Error::new(OLE_E_ADVISENOTSUPPORTED, HSTRING::new()))
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(Error::new(OLE_E_ADVISENOTSUPPORTED, HSTRING::new()))
    }
}

pub fn start_drag<W: HasWindowHandle, F: Fn(DragResult, CursorPosition) + Send + 'static>(
    handle: &W,
    item: DragItem,
    image: Image,
    on_drop_callback: F,
    options: Options,
) -> crate::Result<()> {
    if let Ok(RawWindowHandle::Win32(_w)) = handle.window_handle().map(|h| h.as_raw()) {
        init_ole();
        unsafe {
            #[allow(static_mut_refs)]
            if let Err(e) = &OLE_RESULT {
                return Err(e.clone().into());
            }
        }

        let data_object: IDataObject = match &item {
            DragItem::Files(path_bufs) => {
                // Convert to absolute paths. ILCreateFromPathW doesn't understand UNC paths, so we
                // either need to use dunce::canonicalize or std::path::absolute. dunce resolves
                // links, junction points, etc. so is generally preferred.
                let mut paths = Vec::new();
                for f in path_bufs {
                    paths.push(dunce::canonicalize(f)?);
                }
                get_file_data_object(&paths)
                    .or_else(|| {
                        // dunce converts network locations to UNC paths that ILCreateFromPathW
                        // can't understand, even if it would have been able to parse the original
                        // version. So try path::absolute instead.
                        let mut paths = Vec::new();
                        for f in path_bufs {
                            paths.push(std::path::absolute(f).ok()?);
                        }
                        get_file_data_object(&paths)
                    })
                    // As a last resort, use the HDROP format instead of shell items.
                    .unwrap_or_else(|| DataObject::new(item).into())
            }
            DragItem::Data { .. } => DataObject::new(item).into(),
        };
        let drop_source: IDropSource = DropSource::new().into();

        unsafe {
            if let Some(drag_image) = get_drag_image(image) {
                if let Ok(helper) = create_instance::<IDragSourceHelper>(&CLSID_DragDropHelper) {
                    let _ = helper.InitializeFromBitmap(&drag_image, &data_object);
                }
            }

            let mut out_dropeffect = DROPEFFECT::default();
            let effect = match options.mode {
                DragMode::Copy => DROPEFFECT_COPY,
                DragMode::Move => DROPEFFECT_MOVE,
            };

            let drop_result = DoDragDrop(&data_object, &drop_source, effect, &mut out_dropeffect);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt)?;
            if drop_result == DRAGDROP_S_DROP {
                on_drop_callback(DragResult::Dropped, CursorPosition { x: pt.x, y: pt.y });
            } else {
                // DRAGDROP_S_CANCEL
                on_drop_callback(DragResult::Cancel, CursorPosition { x: pt.x, y: pt.y });
            }
        }
        Ok(())
    } else {
        Err(crate::Error::UnsupportedWindowHandle)
    }
}

fn get_drag_image(image: Image) -> Option<SHDRAGIMAGE> {
    let hbitmap = match image {
        Image::Raw(bytes) => image::read_bytes_to_hbitmap(&bytes).ok(),
        Image::File(path) => image::read_path_to_hbitmap(&path).ok(),
    };
    hbitmap.map(|hbitmap| unsafe {
        // get image size
        let mut bitmap: BITMAP = BITMAP::default();
        let (width, height) = if 0
            == GetObjectW(
                hbitmap,
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bitmap as *mut BITMAP as *mut c_void),
            ) {
            (128, 128)
        } else {
            (bitmap.bmWidth, bitmap.bmHeight)
        };

        SHDRAGIMAGE {
            sizeDragImage: SIZE {
                cx: width,
                cy: height,
            },
            ptOffset: POINT { x: 0, y: 0 },
            hbmpDragImage: hbitmap,
            crColorKey: COLORREF(0x00000000),
        }
    })
}

fn get_hglobal(size: usize, buffer: Vec<u16>) -> Result<HGLOBAL> {
    let handle = unsafe { GlobalAlloc(GMEM_FIXED, size).unwrap() };
    let ptr = unsafe { GlobalLock(handle) };

    let header = ptr as *mut DROPFILES;
    unsafe {
        (*header).pFiles = std::mem::size_of::<DROPFILES>() as u32;
        (*header).fWide = BOOL(1);
        std::ptr::copy(
            buffer.as_ptr() as *const c_void,
            ptr.add(std::mem::size_of::<DROPFILES>()),
            buffer.len() * 2,
        );
        GlobalUnlock(handle)
    }?;
    Ok(handle)
}

pub fn create_instance<T: Interface + ComInterface>(clsid: &GUID) -> Result<T> {
    unsafe { CoCreateInstance(clsid, None, CLSCTX_ALL) }
}

fn get_file_data_object(paths: &[PathBuf]) -> Option<IDataObject> {
    unsafe {
        let shell_item_array = get_shell_item_array(paths).ok()?;
        shell_item_array.BindToHandler(None, &BHID_DataObject).ok()
    }
}

fn get_shell_item_array(paths: &[PathBuf]) -> Result<IShellItemArray> {
    unsafe {
        let list: Vec<*const Common::ITEMIDLIST> = paths
            .iter()
            .map(|path| get_file_item_id(path).cast_const())
            .collect();
        SHCreateShellItemArrayFromIDLists(&list)
    }
}

fn get_file_item_id(path: &Path) -> *mut Common::ITEMIDLIST {
    unsafe {
        let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
        windows::Win32::UI::Shell::ILCreateFromPathW(PCWSTR::from_raw(wide_path.as_ptr()))
    }
}
