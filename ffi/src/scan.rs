//! Scan and EngineData related ffi code

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use delta_kernel::actions::Add;
use delta_kernel::scan::state::{
    visit_scan_files, DvInfo, GlobalScanState as KernelGlobalScanState,
};
use delta_kernel::scan::{Scan as KernelScan, ScanBuilder};
use delta_kernel::{DeltaResult, EngineData};
use tracing::debug;
use url::Url;

use crate::{
    unwrap_kernel_expression, AllocateStringFn, EnginePredicate, ExternEngineInterface,
    ExternEngineInterfaceHandle, ExternResult, IntoExternResult, KernelBoolSlice,
    KernelExpressionVisitorState, KernelStringSlice, SnapshotHandle, TryFromStringSlice,
};

use super::handle::{ArcHandle, BoxHandle};

/// Use a pointer that was created via `BoxHandle::into_handle` or `Box::leak` and returned over FFI
/// without freeing it. returns the result of evaluting `$body`. For example, if `raw_thing` is a
/// pointer previous created by `BoxHandle::into_handle`, one could do the following to return the
/// result of `some_method` _without_ freeing `raw_thing`:
/// ```ignore
/// asbox!(raw_thing as boxed_thing => {
///   boxed_thing.some_method() // some_method defined on the thing in the box
/// })
macro_rules! asbox {
    ($raw_name:ident as $box_name:ident => $body:expr) => {{
        let $box_name = unsafe { Box::from_raw($raw_name) };
        let res = $body;
        // leak the box since we don't want this to free
        Box::leak($box_name);
        res
    }};
}

// TODO: Do we want this type at all? Perhaps we should just _always_ pass raw *mut c_void pointers
// that are the engine data
/// an opaque struct that encapsulates data read by an engine. this handle can be passed back into
/// some kernel calls to operate on the data, or can be converted into the raw data as read by the
/// [`EngineInterface`] by calling [`get_raw_engine_data`]
pub struct EngineDataHandle {
    data: Box<dyn EngineData>,
}
impl BoxHandle for EngineDataHandle {}

/// Allow an engine to "unwrap" an [`EngineDataHandle`] into the raw pointer for the case it wants
/// to use its own engine data format
///
/// # Safety
/// `data_handle` must be a valid pointer to a kernel allocated `EngineDataHandle`
pub unsafe extern "C" fn get_raw_engine_data(data_handle: *mut EngineDataHandle) -> *mut c_void {
    let boxed_data = unsafe { Box::from_raw(data_handle) };
    Box::into_raw(boxed_data.data).cast()
}

/// Struct to allow binding to the arrow [C Data
/// Interface](https://arrow.apache.org/docs/format/CDataInterface.html). This includes the data and
/// the schema.
#[cfg(feature = "default-client")]
#[repr(C)]
pub struct ArrowFFIData {
    array: arrow_data::ffi::FFI_ArrowArray,
    schema: arrow_schema::ffi::FFI_ArrowSchema,
}

/// Get an [`ArrowFFIData`] to allow binding to the arrow [C Data
/// Interface](https://arrow.apache.org/docs/format/CDataInterface.html). This includes the data and
/// the schema.
///
/// # Safety
/// data_handle must be a valid EngineDataHandle as read by the [`DefaultEngineInterface`] obtained
/// from `get_default_client`.
#[cfg(feature = "default-client")]
pub unsafe extern "C" fn get_raw_arrow_data(
    data_handle: *mut EngineDataHandle,
    engine_interface: *const ExternEngineInterfaceHandle,
) -> ExternResult<*mut ArrowFFIData> {
    get_raw_arrow_data_impl(data_handle).into_extern_result(engine_interface)
}

#[cfg(feature = "default-client")]
unsafe fn get_raw_arrow_data_impl(
    data_handle: *mut EngineDataHandle,
) -> DeltaResult<*mut ArrowFFIData> {
    let boxed_data = unsafe { Box::from_raw(data_handle) };
    let data = boxed_data.data;
    let record_batch: arrow_array::RecordBatch = data
        .into_any()
        .downcast::<delta_kernel::client::arrow_data::ArrowEngineData>()
        .map_err(|_| delta_kernel::Error::EngineDataType("ArrowEngineData".to_string()))?
        .into();
    let sa: arrow_array::StructArray = record_batch.into();
    let array_data: arrow_data::ArrayData = sa.into();
    // these call `clone`. is there a way to not copy anything and what exactly are they cloning?
    let array = arrow_data::ffi::FFI_ArrowArray::new(&array_data);
    let schema = arrow_schema::ffi::FFI_ArrowSchema::try_from(array_data.data_type())?;
    let ret_data = Box::new(ArrowFFIData { array, schema });
    Ok(Box::leak(ret_data))
}

/// A scan over some delta data. See the docs for [`delta_kernel::scan::Scan`]
pub struct Scan {
    kernel_scan: KernelScan,
}
impl BoxHandle for Scan {}

/// Get a handle to [`Scan`] over the table specified by the passed snapshot.
/// # Safety
///
/// Caller is responsible for passing a valid snapshot pointer, and engine interface pointer
#[no_mangle]
pub unsafe extern "C" fn scan(
    snapshot: *const SnapshotHandle,
    engine_interface: *const ExternEngineInterfaceHandle,
    predicate: Option<&mut EnginePredicate>,
) -> ExternResult<*mut Scan> {
    scan_impl(snapshot, predicate).into_extern_result(engine_interface)
}

unsafe fn scan_impl(
    snapshot: *const SnapshotHandle,
    predicate: Option<&mut EnginePredicate>,
) -> DeltaResult<*mut Scan> {
    let snapshot = unsafe { ArcHandle::clone_as_arc(snapshot) };
    let mut scan_builder = ScanBuilder::new(snapshot.clone());
    if let Some(predicate) = predicate {
        let mut visitor_state = KernelExpressionVisitorState::new();
        let exprid = (predicate.visitor)(predicate.predicate, &mut visitor_state);
        if let Some(predicate) = unwrap_kernel_expression(&mut visitor_state, exprid) {
            debug!("Got predicate: {}", predicate);
            scan_builder = scan_builder.with_predicate(predicate);
        }
    }
    let kernel_scan = scan_builder.build();
    Ok(BoxHandle::into_handle(Scan { kernel_scan }))
}

pub struct GlobalScanState {
    kernel_state: KernelGlobalScanState,
}
impl BoxHandle for GlobalScanState {}

/// Get the global state for a scan. See the docs for [`delta_kernel::scan::state::GlobalScanState`]
/// for more information.
///
/// # Safety
/// Engine is responsible for providing a valid scan pointer
#[no_mangle]
pub unsafe extern "C" fn get_global_scan_state(scan: *mut Scan) -> *mut GlobalScanState {
    asbox!(scan as boxed_scan => {
        let kernel_state = boxed_scan.kernel_scan.global_scan_state();
        BoxHandle::into_handle(GlobalScanState { kernel_state })
    })
}

/// # Safety
///
/// Caller is responsible for passing a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drop_global_scan_state(state: *mut GlobalScanState) {
    unsafe {
        drop(Box::from_raw(state));
    }
}

// Intentionally opaque to the engine.
#[allow(clippy::type_complexity)]
pub struct KernelScanDataIterator {
    // Box -> Wrap its unsized content this struct is fixed-size with thin pointers.
    // Item = Box<dyn EngineData>, see above, Vec<bool> -> can become a KernelBoolSlice
    data: Box<dyn Iterator<Item = DeltaResult<(Box<dyn EngineData>, Vec<bool>)>>>,

    // Also keep a reference to the external client for its error allocator.
    // Parquet and Json handlers don't hold any reference to the tokio reactor, so the iterator
    // terminates early if the last table client goes out of scope.
    engine_interface: Arc<dyn ExternEngineInterface>,
}

impl BoxHandle for KernelScanDataIterator {}

impl Drop for KernelScanDataIterator {
    fn drop(&mut self) {
        debug!("dropping KernelScanDataIterator");
    }
}

/// Get an iterator over the data needed to perform a scan. This will return a
/// [`KernelScanDataIterator`] which can be passed to [`kernel_scan_data_next`] to get the actual
/// data in the iterator.
///
/// # Safety
///
/// Engine is responsible for passing a valid [`ExternEngineInterfaceHandle`] and [`Scan`]
#[no_mangle]
pub unsafe extern "C" fn kernel_scan_data_init(
    engine_interface: *const ExternEngineInterfaceHandle,
    scan: *mut Scan,
) -> ExternResult<*mut KernelScanDataIterator> {
    kernel_scan_data_init_impl(engine_interface, scan).into_extern_result(engine_interface)
}

unsafe fn kernel_scan_data_init_impl(
    engine_interface: *const ExternEngineInterfaceHandle,
    scan: *mut Scan,
) -> DeltaResult<*mut KernelScanDataIterator> {
    let engine_interface = unsafe { ArcHandle::clone_as_arc(engine_interface) };
    let boxed_scan = unsafe { Box::from_raw(scan) };
    let scan = boxed_scan.kernel_scan;
    let scan_data = scan.scan_data(engine_interface.table_client().as_ref())?;
    let data = KernelScanDataIterator {
        data: Box::new(scan_data),
        engine_interface,
    };
    Ok(data.into_handle())
}

/// # Safety
///
/// The iterator must be valid (returned by [kernel_scan_data_init]) and not yet freed by
/// [kernel_scan_data_free]. The visitor function pointer must be non-null.
#[no_mangle]
pub unsafe extern "C" fn kernel_scan_data_next(
    data: &mut KernelScanDataIterator,
    engine_context: *mut c_void,
    engine_visitor: extern "C" fn(
        engine_context: *mut c_void,
        engine_data: *mut EngineDataHandle,
        selection_vector: KernelBoolSlice,
    ),
) -> ExternResult<bool> {
    kernel_scan_data_next_impl(data, engine_context, engine_visitor)
        .into_extern_result(data.engine_interface.error_allocator())
}
fn kernel_scan_data_next_impl(
    data: &mut KernelScanDataIterator,
    engine_context: *mut c_void,
    engine_visitor: extern "C" fn(
        engine_context: *mut c_void,
        engine_data: *mut EngineDataHandle,
        selection_vector: KernelBoolSlice,
    ),
) -> DeltaResult<bool> {
    if let Some((data, sel_vec)) = data.data.next().transpose()? {
        let bool_slice: KernelBoolSlice = sel_vec.into();
        let data_handle = BoxHandle::into_handle(EngineDataHandle { data });
        (engine_visitor)(engine_context, data_handle, bool_slice);
        // ensure we free the data
        unsafe { BoxHandle::drop_handle(data_handle) };
        Ok(true)
    } else {
        Ok(false)
    }
}

/// # Safety
///
/// Caller is responsible for (at most once) passing a valid pointer returned by a call to
/// [kernel_scan_files_init].
// we should probably be consistent with drop vs. free on engine side (probably the latter is more
// intuitive to non-rust code)
#[no_mangle]
pub unsafe extern "C" fn kernel_scan_data_free(data: *mut KernelScanDataIterator) {
    BoxHandle::drop_handle(data);
}

type CScanCallback = extern "C" fn(
    engine_context: *mut c_void,
    path: KernelStringSlice,
    size: i64,
    dv_info: *mut CDvInfo,
    partition_map: *mut CStringMap,
);

pub struct CDvInfo {
    dv_info: DvInfo,
}
impl BoxHandle for CDvInfo {}

pub struct CStringMap {
    values: HashMap<String, String>,
}
impl BoxHandle for CStringMap {}

#[no_mangle]
/// allow probing into a CStringMap. If the specified key is in the map, kernel will call
/// allocate_fn with the value associated with the key and return the value returned from that
/// function. If the key is not in the map, this will return NULL
///
/// # Safety
///
/// The engine is responsible for providing a valid [`CStringMap`] pointer and [`KernelStringSlice`]
pub unsafe extern "C" fn get_from_map(
    raw_map: *mut CStringMap,
    key: KernelStringSlice,
    allocate_fn: AllocateStringFn,
) -> *mut c_void {
    asbox!(raw_map as boxed_map => {
        let string_key = String::try_from_slice(key);
        match boxed_map.values.get(&string_key) {
            Some(v) => {
                let slice: KernelStringSlice = v.as_str().into();
                allocate_fn(slice)
            }
            None => std::ptr::null_mut(),
        }
    })
}

/// Get a selection vector out of a [`CDvInfo`] struct
///
/// # Safety
/// Engine is responsible for providing valid pointers for each argument
#[no_mangle]
pub unsafe extern "C" fn selection_vector_from_dv(
    raw_info: *mut CDvInfo,
    extern_engine_interface: *const ExternEngineInterfaceHandle,
    state: *mut GlobalScanState,
) -> *mut KernelBoolSlice {
    asbox!(raw_info as boxed_info => {
        asbox!(state as boxed_state => {
            let extern_engine_interface = unsafe { ArcHandle::clone_as_arc(extern_engine_interface) };
            let root_url = Url::parse(&boxed_state.kernel_state.table_root).unwrap();
            let vopt = boxed_info
                .dv_info
                .get_selection_vector(extern_engine_interface.table_client().as_ref(), &root_url)
                .unwrap();
            match vopt {
                Some(v) => Box::into_raw(Box::new(v.into())),
                None => std::ptr::null_mut(),
            }
        })
    })
}

// Wrapper function that gets called by the kernel, transforms the arguments to make the ffi-able,
// and then calls the ffi specified callback
fn rust_callback(
    context: &mut ContextWrapper,
    path: &str,
    size: i64,
    dv_info: DvInfo,
    partition_values: HashMap<String, String>,
) {
    let path_slice: KernelStringSlice = path.into();
    let dv_handle = BoxHandle::into_handle(CDvInfo { dv_info });
    let partition_map_handle = BoxHandle::into_handle(CStringMap {
        values: partition_values,
    });
    (context.callback)(
        context.engine_context,
        path_slice,
        size,
        dv_handle,
        partition_map_handle,
    );
    unsafe {
        BoxHandle::drop_handle(dv_handle);
    }
}

// Wrap up stuff from C so we can pass it through to our callback
struct ContextWrapper {
    engine_context: *mut c_void,
    callback: CScanCallback,
}

/// Shim for ffi to call visit_scan_data. This will generally be called when iterating through scan
/// data which provides the data handle and selection vector as each element in the iterator.
///
/// # Safety
/// engine is responsbile for passing a valid [`EngineDataHandle`] and selection vector.
#[no_mangle]
pub unsafe extern "C" fn visit_scan_data(
    data: *mut EngineDataHandle,
    vector: KernelBoolSlice,
    engine_context: *mut c_void,
    callback: CScanCallback,
) {
    let selection_vec = vector.make_vec();
    let data: &dyn EngineData = unsafe { (*data).data.as_ref() };
    let context_wrapper = ContextWrapper {
        engine_context,
        callback,
    };
    visit_scan_files(data, selection_vec.clone(), context_wrapper, rust_callback).unwrap();
    Box::new(selection_vec).leak();
}

// Intentionally opaque to the engine.
pub struct KernelScanFileIterator {
    // Box -> Wrap its unsized content this struct is fixed-size with thin pointers.
    // Item = String -> Owned items because rust can't correctly express lifetimes for borrowed items
    // (we would need a way to assert that item lifetimes are bounded by the iterator's lifetime).
    files: Box<dyn Iterator<Item = DeltaResult<Add>>>,

    // Also keep a reference to the external client for its error allocator.
    // Parquet and Json handlers don't hold any reference to the tokio reactor, so the iterator
    // terminates early if the last table client goes out of scope.
    table_client: Arc<dyn ExternEngineInterface>,
}

impl BoxHandle for KernelScanFileIterator {}

impl Drop for KernelScanFileIterator {
    fn drop(&mut self) {
        debug!("dropping KernelScanFileIterator");
    }
}

/// Get a FileList for all the files that need to be read from the table.
/// # Safety
///
/// Caller is responsible for passing a valid snapshot pointer.
#[no_mangle]
pub unsafe extern "C" fn kernel_scan_files_init(
    snapshot: *const SnapshotHandle,
    table_client: *const ExternEngineInterfaceHandle,
    predicate: Option<&mut EnginePredicate>,
) -> ExternResult<*mut KernelScanFileIterator> {
    kernel_scan_files_init_impl(snapshot, table_client, predicate).into_extern_result(table_client)
}

fn kernel_scan_files_init_impl(
    snapshot: *const SnapshotHandle,
    extern_table_client: *const ExternEngineInterfaceHandle,
    predicate: Option<&mut EnginePredicate>,
) -> DeltaResult<*mut KernelScanFileIterator> {
    let snapshot = unsafe { ArcHandle::clone_as_arc(snapshot) };
    let extern_table_client = unsafe { ArcHandle::clone_as_arc(extern_table_client) };
    let mut scan_builder = ScanBuilder::new(snapshot.clone());
    if let Some(predicate) = predicate {
        // TODO: There is a lot of redundancy between the various visit_expression_XXX methods here,
        // vs. ProvidesMetadataFilter trait and the class hierarchy that supports it. Can we justify
        // combining the two, so that native rust kernel code also uses the visitor idiom? Doing so
        // might mean kernel no longer needs to define an expression class hierarchy of its own (at
        // least, not for data skipping). Things may also look different after we remove arrow code
        // from the kernel proper and make it one of the sensible default engine clients instead.
        let mut visitor_state = KernelExpressionVisitorState::new();
        let exprid = (predicate.visitor)(predicate.predicate, &mut visitor_state);
        if let Some(predicate) = unwrap_kernel_expression(&mut visitor_state, exprid) {
            println!("Got predicate: {}", predicate);
            scan_builder = scan_builder.with_predicate(predicate);
        }
    }
    let scan_adds = scan_builder
        .build()
        .files(extern_table_client.table_client().as_ref())?;
    let files = KernelScanFileIterator {
        files: Box::new(scan_adds),
        table_client: extern_table_client,
    };
    Ok(files.into_handle())
}

/// # Safety
///
/// The iterator must be valid (returned by [kernel_scan_files_init]) and not yet freed by
/// [kernel_scan_files_free]. The visitor function pointer must be non-null.
#[no_mangle]
pub unsafe extern "C" fn kernel_scan_files_next(
    files: &mut KernelScanFileIterator,
    engine_context: *mut c_void,
    engine_visitor: extern "C" fn(engine_context: *mut c_void, file_name: KernelStringSlice),
) -> ExternResult<bool> {
    kernel_scan_files_next_impl(files, engine_context, engine_visitor)
        .into_extern_result(files.table_client.error_allocator())
}
fn kernel_scan_files_next_impl(
    files: &mut KernelScanFileIterator,
    engine_context: *mut c_void,
    engine_visitor: extern "C" fn(engine_context: *mut c_void, file_name: KernelStringSlice),
) -> DeltaResult<bool> {
    if let Some(add) = files.files.next().transpose()? {
        debug!("Got file: {}", add.path);
        (engine_visitor)(engine_context, add.path.as_str().into());
        Ok(true)
    } else {
        Ok(false)
    }
}

/// # Safety
///
/// Caller is responsible for (at most once) passing a valid pointer returned by a call to
/// [kernel_scan_files_init].
// we should probably be consistent with drop vs. free on engine side (probably the latter is more
// intuitive to non-rust code)
#[no_mangle]
pub unsafe extern "C" fn kernel_scan_files_free(files: *mut KernelScanFileIterator) {
    BoxHandle::drop_handle(files);
}