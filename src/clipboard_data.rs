//! A clipboard data object for copies that must also paste as a file.
//!
//! Explorer reads a virtual file's bytes with `FileContents` at `lindex` 0,
//! which data placed with `SetClipboardData` cannot answer. This object
//! serves every format from memory and goes on the clipboard through
//! `OleSetClipboard`; OLE renders the plain formats for other apps.

use windows::{
    core::{implement, Ref, Result, BOOL, HRESULT},
    Win32::{
        Foundation::{
            GlobalFree, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_NOTIMPL, HGLOBAL,
            OLE_E_ADVISENOTSUPPORTED, S_OK,
        },
        System::{
            Com::{
                IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR_GET,
                DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
            },
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            Ole::{OleInitialize, OleSetClipboard},
        },
        UI::Shell::SHCreateStdEnumFmtEtc,
    },
};

#[implement(IDataObject)]
struct ClipboardData {
    items: Vec<(u16, Vec<u8>)>,
    /// The format whose data may be asked for at `lindex` 0.
    file_contents: u16,
}

impl ClipboardData {
    fn find(&self, format: &FORMATETC) -> std::result::Result<&[u8], HRESULT> {
        let (_, bytes) = self
            .items
            .iter()
            .find(|(id, _)| *id == format.cfFormat)
            .ok_or(DV_E_FORMATETC)?;
        if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(DV_E_TYMED);
        }
        let index_ok = format.lindex == -1 || (format.lindex == 0 && format.cfFormat == self.file_contents);
        if !index_ok {
            return Err(DV_E_LINDEX);
        }
        Ok(bytes)
    }
}

fn format_etc(id: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

unsafe fn global_copy(bytes: &[u8]) -> Result<HGLOBAL> {
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
    let target = GlobalLock(memory).cast::<u8>();
    if target.is_null() {
        let _ = GlobalFree(Some(memory));
        return Err(windows::core::Error::from_thread());
    }
    target.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
    let _ = GlobalUnlock(memory);
    Ok(memory)
}

#[allow(non_snake_case)]
impl IDataObject_Impl for ClipboardData_Impl {
    fn GetData(&self, format: *const FORMATETC) -> Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(DV_E_FORMATETC)?;
        let bytes = self.find(format)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: unsafe { global_copy(bytes)? } },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        match unsafe { format.as_ref() } {
            Some(format) => self.find(format).map_or_else(|error| error, |_| S_OK),
            None => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    /// Explorer reports "Performed DropEffect" after pasting; nothing to keep.
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats: Vec<FORMATETC> = self.items.iter().map(|(id, _)| format_etc(*id)).collect();
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Puts `items` (clipboard format, bytes) on the clipboard; `file_contents`
/// is the format Explorer reads at `lindex` 0. Must run on a UI thread.
pub fn set(items: Vec<(u32, Vec<u8>)>, file_contents: u32) -> std::result::Result<(), String> {
    unsafe {
        // OLE needs an apartment on this thread; repeated calls are cheap.
        let _ = OleInitialize(None);
        let data: IDataObject = ClipboardData {
            items: items.into_iter().map(|(id, bytes)| (id as u16, bytes)).collect(),
            file_contents: file_contents as u16,
        }
        .into();
        OleSetClipboard(&data).map_err(|error| error.message())
    }
}
