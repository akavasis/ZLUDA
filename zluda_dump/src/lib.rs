use std::{
    collections::HashMap,
    env,
    error::Error,
    ffi::{c_void, CStr},
    fs,
    io::prelude::*,
    mem,
    os::raw::{c_int, c_uint, c_ulong, c_ushort},
    path::PathBuf,
    rc::Rc,
    slice,
};
use std::{fs::File, ptr};

use cuda::{CUdeviceptr, CUfunction, CUjit_option, CUmodule, CUresult, CUstream, CUuuid};
use ptx::ast;
use regex::Regex;

#[cfg_attr(windows, path = "os_win.rs")]
#[cfg_attr(not(windows), path = "os_unix.rs")]
mod os;

macro_rules! extern_redirect {
    (pub fn $fn_name:ident ( $($arg_id:ident: $arg_type:ty),* $(,)? ) -> $ret_type:ty ;) => {
        #[no_mangle]
        pub fn $fn_name ( $( $arg_id : $arg_type),* ) -> $ret_type {
            unsafe { $crate::init_libcuda_handle() };
            let name = std::ffi::CString::new(stringify!($fn_name)).unwrap();
            let fn_ptr = unsafe { crate::os::get_proc_address($crate::LIBCUDA_HANDLE, &name) };
            if fn_ptr == std::ptr::null_mut() {
                return CUresult::CUDA_ERROR_UNKNOWN;
            }
            let typed_fn = unsafe { std::mem::transmute::<_, fn( $( $arg_id : $arg_type),* ) -> $ret_type>(fn_ptr) };
            typed_fn($( $arg_id ),*)
        }
    };
}

macro_rules! extern_redirect_with {
    (
        pub fn $fn_name:ident ( $($arg_id:ident: $arg_type:ty),* $(,)? ) -> $ret_type:ty ;
        $receiver:path ;
    ) => {
        #[no_mangle]
        pub fn $fn_name ( $( $arg_id : $arg_type),* ) -> $ret_type {
            unsafe { $crate::init_libcuda_handle() };
            let continuation = |$( $arg_id : $arg_type),* | {
                let name = std::ffi::CString::new(stringify!($fn_name)).unwrap();
                let fn_ptr = unsafe { crate::os::get_proc_address($crate::LIBCUDA_HANDLE, &name) };
                if fn_ptr == std::ptr::null_mut() {
                    return CUresult::CUDA_ERROR_UNKNOWN;
                }
                let typed_fn = unsafe { std::mem::transmute::<_, fn( $( $arg_id : $arg_type),* ) -> $ret_type>(fn_ptr) };
                typed_fn($( $arg_id ),*)
            };
            unsafe { $receiver($( $arg_id ),* , continuation) }
        }
    };
}

#[allow(warnings)]
mod cuda;

pub static mut LIBCUDA_HANDLE: *mut c_void = ptr::null_mut();
pub static mut MODULES: Option<HashMap<CUmodule, ModuleDump>> = None;
pub static mut KERNELS: Option<HashMap<CUfunction, KernelDump>> = None;
pub static mut BUFFERS: Vec<(usize, usize)> = Vec::new();
pub static mut LAUNCH_COUNTER: usize = 0;
pub static mut KERNEL_PATTERN: Option<Regex> = None;

pub struct ModuleDump {
    content: Rc<String>,
    kernels_args: HashMap<String, Vec<usize>>,
}

pub struct KernelDump {
    module_content: Rc<String>,
    name: String,
    arguments: Vec<usize>,
}

// We are doing dlopen here instead of just using LD_PRELOAD,
// it's because CUDA Runtime API does dlopen to open libcuda.so, which ignores LD_PRELOAD
pub unsafe fn init_libcuda_handle() {
    if LIBCUDA_HANDLE == ptr::null_mut() {
        let libcuda_handle = os::load_cuda_library();
        assert_ne!(libcuda_handle, ptr::null_mut());
        LIBCUDA_HANDLE = libcuda_handle;
        match env::var("ZLUDA_DUMP_KERNEL") {
            Ok(kernel_filter) => match Regex::new(&kernel_filter) {
                Ok(r) => KERNEL_PATTERN = Some(r),
                Err(err) => {
                    eprintln!(
                        "[ZLUDA_DUMP] Env variable ZLUDA_DUMP_KERNEL is not a regex: {}",
                        err
                    );
                }
            },
            Err(_) => (),
        }
        eprintln!("[ZLUDA_DUMP] Initialized");
    }
}

#[allow(non_snake_case)]
pub unsafe fn cuModuleLoadData(
    module: *mut CUmodule,
    raw_image: *const ::std::os::raw::c_void,
    cont: impl FnOnce(*mut CUmodule, *const c_void) -> CUresult,
) -> CUresult {
    let result = cont(module, raw_image);
    if result == CUresult::CUDA_SUCCESS {
        record_module_image_raw(*module, raw_image);
    }
    result
}

unsafe fn record_module_image_raw(module: CUmodule, raw_image: *const ::std::os::raw::c_void) {
    let image = to_str(raw_image);
    match image {
        None => eprintln!("[ZLUDA_DUMP] Malformed module image: {:?}", raw_image),
        Some(image) => record_module_image(module, image),
    };
}

unsafe fn record_module_image(module: CUmodule, image: &str) {
    if !image.contains(&".address_size") {
        eprintln!("[ZLUDA_DUMP] Malformed module image: {:?}", module)
    } else {
        let mut errors = Vec::new();
        let ast = ptx::ModuleParser::new().parse(&mut errors, image);
        match (&*errors, ast) {
            (&[], Ok(ast)) => {
                let kernels_args = ast
                    .directives
                    .iter()
                    .filter_map(directive_to_kernel)
                    .collect::<HashMap<_, _>>();
                let modules = MODULES.get_or_insert_with(|| HashMap::new());
                modules.insert(
                    module,
                    ModuleDump {
                        content: Rc::new(image.to_string()),
                        kernels_args,
                    },
                );
            }
            (errs, ast) => {
                let err_string = errs
                    .iter()
                    .map(|e| format!("{:?}", e))
                    .chain(ast.err().iter().map(|e| format!("{:?}", e)))
                    .collect::<Vec<_>>()
                    .join("\n");
                eprintln!(
                    "[ZLUDA_DUMP] Errors when parsing module:\n---ERRORS---\n{}\n---MODULE---\n{}",
                    err_string, image
                );
            }
        }
    }
}

unsafe fn to_str<T>(image: *const T) -> Option<&'static str> {
    let ptr = image as *const u8;
    let mut offset = 0;
    loop {
        let c = *ptr.add(offset);
        if !c.is_ascii() {
            return None;
        }
        if c == 0 {
            return Some(std::str::from_utf8_unchecked(slice::from_raw_parts(
                ptr, offset,
            )));
        }
        offset += 1;
    }
}

fn directive_to_kernel(dir: &ast::Directive<ast::ParsedArgParams>) -> Option<(String, Vec<usize>)> {
    match dir {
        ast::Directive::Method(ast::Function {
            func_directive: ast::MethodDecl::Kernel { name, in_args },
            ..
        }) => {
            let arg_sizes = in_args
                .iter()
                .map(|arg| ast::Type::from(arg.v_type.clone()).size_of())
                .collect();
            Some((name.to_string(), arg_sizes))
        }
        _ => None,
    }
}

#[allow(non_snake_case)]
pub unsafe fn cuModuleLoadDataEx(
    module: *mut CUmodule,
    image: *const c_void,
    numOptions: c_uint,
    options: *mut CUjit_option,
    optionValues: *mut *mut c_void,
    cont: impl FnOnce(
        *mut CUmodule,
        *const c_void,
        c_uint,
        *mut CUjit_option,
        *mut *mut c_void,
    ) -> CUresult,
) -> CUresult {
    let result = cont(module, image, numOptions, options, optionValues);
    if result == CUresult::CUDA_SUCCESS {
        record_module_image_raw(*module, image);
    }
    result
}

#[allow(non_snake_case)]
unsafe fn cuModuleGetFunction(
    hfunc: *mut CUfunction,
    hmod: CUmodule,
    name: *const ::std::os::raw::c_char,
    cont: impl FnOnce(*mut CUfunction, CUmodule, *const ::std::os::raw::c_char) -> CUresult,
) -> CUresult {
    let result = cont(hfunc, hmod, name);
    if result != CUresult::CUDA_SUCCESS {
        return result;
    }
    if let Some(modules) = &MODULES {
        if let Some(module_dump) = modules.get(&hmod) {
            if let Some(kernel) = to_str(name) {
                if let Some(args) = module_dump.kernels_args.get(kernel) {
                    let kernel_args = KERNELS.get_or_insert_with(|| HashMap::new());
                    kernel_args.insert(
                        *hfunc,
                        KernelDump {
                            module_content: module_dump.content.clone(),
                            name: kernel.to_string(),
                            arguments: args.clone(),
                        },
                    );
                } else {
                    eprintln!("[ZLUDA_DUMP] Unknown kernel: {}", kernel);
                }
            } else {
                eprintln!("[ZLUDA_DUMP] Unknown kernel name at: {:?}", hfunc);
            }
        } else {
            eprintln!("[ZLUDA_DUMP] Unknown module: {:?}", hmod);
        }
    } else {
        eprintln!("[ZLUDA_DUMP] Unknown module: {:?}", hmod);
    }
    CUresult::CUDA_SUCCESS
}

#[allow(non_snake_case)]
pub unsafe fn cuMemAlloc_v2(
    dptr: *mut CUdeviceptr,
    bytesize: usize,
    cont: impl FnOnce(*mut CUdeviceptr, usize) -> CUresult,
) -> CUresult {
    let result = cont(dptr, bytesize);
    assert_eq!(result, CUresult::CUDA_SUCCESS);
    let start = (*dptr).0 as usize;
    BUFFERS.push((start, bytesize));
    CUresult::CUDA_SUCCESS
}

#[allow(non_snake_case)]
pub unsafe fn cuLaunchKernel(
    f: CUfunction,
    gridDimX: ::std::os::raw::c_uint,
    gridDimY: ::std::os::raw::c_uint,
    gridDimZ: ::std::os::raw::c_uint,
    blockDimX: ::std::os::raw::c_uint,
    blockDimY: ::std::os::raw::c_uint,
    blockDimZ: ::std::os::raw::c_uint,
    sharedMemBytes: ::std::os::raw::c_uint,
    hStream: CUstream,
    kernelParams: *mut *mut ::std::os::raw::c_void,
    extra: *mut *mut ::std::os::raw::c_void,
    cont: impl FnOnce(
        CUfunction,
        ::std::os::raw::c_uint,
        ::std::os::raw::c_uint,
        ::std::os::raw::c_uint,
        ::std::os::raw::c_uint,
        ::std::os::raw::c_uint,
        ::std::os::raw::c_uint,
        ::std::os::raw::c_uint,
        CUstream,
        *mut *mut ::std::os::raw::c_void,
        *mut *mut ::std::os::raw::c_void,
    ) -> CUresult,
) -> CUresult {
    let mut error;
    let dump_env = match create_dump_dir(f, LAUNCH_COUNTER) {
        Ok(dump_env) => dump_env,
        Err(err) => {
            eprintln!("[ZLUDA_DUMP] {:#?}", err);
            None
        }
    };
    if let Some(dump_env) = &dump_env {
        dump_pre_data(
            gridDimX,
            gridDimY,
            gridDimZ,
            blockDimX,
            blockDimY,
            blockDimZ,
            sharedMemBytes,
            kernelParams,
            dump_env,
        )
        .unwrap_or_else(|err| eprintln!("[ZLUDA_DUMP] {:#?}", err));
    };
    error = cont(
        f,
        gridDimX,
        gridDimY,
        gridDimZ,
        blockDimX,
        blockDimY,
        blockDimZ,
        sharedMemBytes,
        hStream,
        kernelParams,
        extra,
    );
    assert_eq!(error, CUresult::CUDA_SUCCESS);
    error = cuda::cuStreamSynchronize(hStream);
    assert_eq!(error, CUresult::CUDA_SUCCESS);
    if let Some((_, kernel_dump)) = &dump_env {
        dump_arguments(
            kernelParams,
            "post",
            &kernel_dump.name,
            LAUNCH_COUNTER,
            &kernel_dump.arguments,
        )
        .unwrap_or_else(|err| eprintln!("[ZLUDA_DUMP] {:#?}", err));
    }
    LAUNCH_COUNTER += 1;
    CUresult::CUDA_SUCCESS
}

#[allow(non_snake_case)]
fn dump_launch_arguments(
    gridDimX: u32,
    gridDimY: u32,
    gridDimZ: u32,
    blockDimX: u32,
    blockDimY: u32,
    blockDimZ: u32,
    sharedMemBytes: u32,
    dump_dir: &PathBuf,
) -> Result<(), Box<dyn Error>> {
    let mut module_file_path = dump_dir.clone();
    module_file_path.push("launch.txt");
    let mut module_file = File::create(module_file_path)?;
    write!(&mut module_file, "{}\n", gridDimX)?;
    write!(&mut module_file, "{}\n", gridDimY)?;
    write!(&mut module_file, "{}\n", gridDimZ)?;
    write!(&mut module_file, "{}\n", blockDimX)?;
    write!(&mut module_file, "{}\n", blockDimY)?;
    write!(&mut module_file, "{}\n", blockDimZ)?;
    write!(&mut module_file, "{}\n", sharedMemBytes)?;
    Ok(())
}

unsafe fn should_dump_kernel(name: &str) -> bool {
    match &KERNEL_PATTERN {
        Some(pattern) => pattern.is_match(name),
        None => true,
    }
}

unsafe fn create_dump_dir(
    f: CUfunction,
    counter: usize,
) -> Result<Option<(PathBuf, &'static KernelDump)>, Box<dyn Error>> {
    match KERNELS.as_ref().and_then(|kernels| kernels.get(&f)) {
        Some(kernel_dump) => {
            if !should_dump_kernel(&kernel_dump.name) {
                return Ok(None);
            }
            let mut dump_dir = get_dump_dir()?;
            dump_dir.push(format!("{:04}_{}", counter, kernel_dump.name));
            fs::create_dir_all(&dump_dir)?;
            Ok(Some((dump_dir, kernel_dump)))
        }
        None => Err("Unknown kernel: {:?}")?,
    }
}

#[allow(non_snake_case)]
unsafe fn dump_pre_data(
    gridDimX: ::std::os::raw::c_uint,
    gridDimY: ::std::os::raw::c_uint,
    gridDimZ: ::std::os::raw::c_uint,
    blockDimX: ::std::os::raw::c_uint,
    blockDimY: ::std::os::raw::c_uint,
    blockDimZ: ::std::os::raw::c_uint,
    sharedMemBytes: ::std::os::raw::c_uint,
    kernelParams: *mut *mut ::std::os::raw::c_void,
    (dump_dir, kernel_dump): &(PathBuf, &'static KernelDump),
) -> Result<(), Box<dyn Error>> {
    dump_launch_arguments(
        gridDimX,
        gridDimY,
        gridDimZ,
        blockDimX,
        blockDimY,
        blockDimZ,
        sharedMemBytes,
        dump_dir,
    )?;
    let mut module_file_path = dump_dir.clone();
    module_file_path.push("module.ptx");
    let mut module_file = File::create(module_file_path)?;
    module_file.write_all(kernel_dump.module_content.as_bytes())?;
    dump_arguments(
        kernelParams,
        "pre",
        &kernel_dump.name,
        LAUNCH_COUNTER,
        &kernel_dump.arguments,
    )?;
    Ok(())
}

unsafe fn dump_arguments(
    kernel_params: *mut *mut ::std::os::raw::c_void,
    prefix: &str,
    kernel_name: &str,
    counter: usize,
    args: &[usize],
) -> Result<(), Box<dyn Error>> {
    let mut dump_dir = get_dump_dir()?;
    dump_dir.push(format!("{:04}_{}", counter, kernel_name));
    dump_dir.push(prefix);
    if dump_dir.exists() {
        fs::remove_dir_all(&dump_dir)?;
    }
    fs::create_dir_all(&dump_dir)?;
    for (i, arg_len) in args.iter().enumerate() {
        let dev_ptr = *(*kernel_params.add(i) as *mut usize);
        match BUFFERS.iter().find(|(start, _)| *start == dev_ptr as usize) {
            Some((start, len)) => {
                let mut output = vec![0u8; *len];
                let error =
                    cuda::cuMemcpyDtoH_v2(output.as_mut_ptr() as *mut _, CUdeviceptr(*start), *len);
                assert_eq!(error, CUresult::CUDA_SUCCESS);
                let mut path = dump_dir.clone();
                path.push(format!("arg_{:03}.buffer", i));
                let mut file = File::create(path)?;
                file.write_all(&mut output)?;
            }
            None => {
                let mut path = dump_dir.clone();
                path.push(format!("arg_{:03}", i));
                let mut file = File::create(path)?;
                file.write_all(slice::from_raw_parts(
                    *kernel_params.add(i) as *mut u8,
                    *arg_len,
                ))?;
            }
        }
    }
    Ok(())
}

fn get_dump_dir() -> Result<PathBuf, Box<dyn Error>> {
    let dir = env::var("ZLUDA_DUMP_DIR")?;
    let mut main_dir = PathBuf::from(dir);
    let current_exe = env::current_exe()?;
    main_dir.push(current_exe.file_name().unwrap());
    fs::create_dir_all(&main_dir)?;
    Ok(main_dir)
}

// TODO make this more common with ZLUDA implementation
const CUDART_INTERFACE_GUID: CUuuid = CUuuid {
    bytes: [
        0x6b, 0xd5, 0xfb, 0x6c, 0x5b, 0xf4, 0xe7, 0x4a, 0x89, 0x87, 0xd9, 0x39, 0x12, 0xfd, 0x9d,
        0xf9,
    ],
};

const GET_MODULE_OFFSET: usize = 6;
static mut CUDART_INTERFACE_VTABLE: Vec<*const c_void> = Vec::new();
static mut ORIGINAL_GET_MODULE_FROM_CUBIN: Option<
    unsafe extern "C" fn(
        result: *mut CUmodule,
        fatbinc_wrapper: *const FatbincWrapper,
        ptr1: *mut c_void,
        ptr2: *mut c_void,
    ) -> CUresult,
> = None;

#[allow(non_snake_case)]
pub unsafe fn cuGetExportTable(
    ppExportTable: *mut *const ::std::os::raw::c_void,
    pExportTableId: *const CUuuid,
    cont: impl FnOnce(*mut *const ::std::os::raw::c_void, *const CUuuid) -> CUresult,
) -> CUresult {
    if *pExportTableId == CUDART_INTERFACE_GUID {
        if CUDART_INTERFACE_VTABLE.len() == 0 {
            let mut base_table = ptr::null();
            let base_result = cont(&mut base_table, pExportTableId);
            if base_result != CUresult::CUDA_SUCCESS {
                return base_result;
            }
            let len = *(base_table as *const usize);
            CUDART_INTERFACE_VTABLE = vec![ptr::null(); len];
            ptr::copy_nonoverlapping(
                base_table as *const _,
                CUDART_INTERFACE_VTABLE.as_mut_ptr(),
                len,
            );
            if GET_MODULE_OFFSET >= len {
                return CUresult::CUDA_ERROR_UNKNOWN;
            }
            ORIGINAL_GET_MODULE_FROM_CUBIN =
                mem::transmute(CUDART_INTERFACE_VTABLE[GET_MODULE_OFFSET]);
            CUDART_INTERFACE_VTABLE[GET_MODULE_OFFSET] = get_module_from_cubin as *const _;
        }
        *ppExportTable = CUDART_INTERFACE_VTABLE.as_ptr() as *const _;
        return CUresult::CUDA_SUCCESS;
    } else {
        cont(ppExportTable, pExportTableId)
    }
}

const FATBINC_MAGIC: c_uint = 0x466243B1;
const FATBINC_VERSION: c_uint = 0x1;

#[repr(C)]
struct FatbincWrapper {
    magic: c_uint,
    version: c_uint,
    data: *const FatbinHeader,
    filename_or_fatbins: *const c_void,
}

const FATBIN_MAGIC: c_uint = 0xBA55ED50;
const FATBIN_VERSION: c_ushort = 0x01;

#[repr(C, align(8))]
struct FatbinHeader {
    magic: c_uint,
    version: c_ushort,
    header_size: c_ushort,
    files_size: c_ulong, // excluding frame header, size of all blocks framed by this frame
}

const FATBIN_FILE_HEADER_KIND_PTX: c_ushort = 0x01;
const FATBIN_FILE_HEADER_VERSION_CURRENT: c_ushort = 0x101;

// assembly file header is a bit different, but we don't care
#[repr(C)]
#[derive(Debug)]
struct FatbinFileHeader {
    kind: c_ushort,
    version: c_ushort,
    header_size: c_uint,
    padded_payload_size: c_uint,
    unknown0: c_uint, // check if it's written into separately
    payload_size: c_uint,
    unknown1: c_uint,
    unknown2: c_uint,
    sm_version: c_uint,
    bit_width: c_uint,
    unknown3: c_uint,
    unknown4: c_ulong,
    unknown5: c_ulong,
    uncompressed_payload: c_ulong,
}

unsafe extern "C" fn get_module_from_cubin(
    module: *mut CUmodule,
    fatbinc_wrapper: *const FatbincWrapper,
    ptr1: *mut c_void,
    ptr2: *mut c_void,
) -> CUresult {
    if module == ptr::null_mut()
        || (*fatbinc_wrapper).magic != FATBINC_MAGIC
        || (*fatbinc_wrapper).version != FATBINC_VERSION
    {
        return CUresult::CUDA_ERROR_INVALID_VALUE;
    }
    let fatbin_header = (*fatbinc_wrapper).data;
    if (*fatbin_header).magic != FATBIN_MAGIC || (*fatbin_header).version != FATBIN_VERSION {
        return CUresult::CUDA_ERROR_INVALID_VALUE;
    }
    let file = (fatbin_header as *const u8).add((*fatbin_header).header_size as usize);
    let end = file.add((*fatbin_header).files_size as usize);
    let mut ptx_files = get_ptx_files(file, end);
    ptx_files.sort_unstable_by_key(|f| c_uint::max_value() - (**f).sm_version);
    let mut maybe_kernel_text = None;
    for file in ptx_files {
        match decompress_kernel_module(file) {
            None => continue,
            Some(vec) => {
                maybe_kernel_text = Some(vec);
                break;
            }
        };
    }
    let result = ORIGINAL_GET_MODULE_FROM_CUBIN.unwrap()(module, fatbinc_wrapper, ptr1, ptr2);
    if result != CUresult::CUDA_SUCCESS {
        return result;
    }
    if let Some(text) = maybe_kernel_text {
        match CStr::from_bytes_with_nul(&text) {
            Ok(cstr) => match cstr.to_str() {
                Ok(utf8_str) => record_module_image(*module, utf8_str),
                Err(_) => {}
            },
            Err(_) => {}
        }
    }
    result
}

unsafe fn get_ptx_files(file: *const u8, end: *const u8) -> Vec<*const FatbinFileHeader> {
    let mut index = file;
    let mut result = Vec::new();
    while index < end {
        let file = index as *const FatbinFileHeader;
        if (*file).kind == FATBIN_FILE_HEADER_KIND_PTX
            && (*file).version == FATBIN_FILE_HEADER_VERSION_CURRENT
        {
            result.push(file)
        }
        index = index.add((*file).header_size as usize + (*file).padded_payload_size as usize);
    }
    result
}

const MAX_PTX_MODULE_DECOMPRESSION_BOUND: usize = 16 * 1024 * 1024;

unsafe fn decompress_kernel_module(file: *const FatbinFileHeader) -> Option<Vec<u8>> {
    let decompressed_size = usize::max(1024, (*file).uncompressed_payload as usize);
    let mut decompressed_vec = vec![0u8; decompressed_size];
    loop {
        match lz4_sys::LZ4_decompress_safe(
            (file as *const u8).add((*file).header_size as usize) as *const _,
            decompressed_vec.as_mut_ptr() as *mut _,
            (*file).payload_size as c_int,
            decompressed_vec.len() as c_int,
        ) {
            error if error < 0 => {
                let new_size = decompressed_vec.len() * 2;
                if new_size > MAX_PTX_MODULE_DECOMPRESSION_BOUND {
                    return None;
                }
                decompressed_vec.resize(decompressed_vec.len() * 2, 0);
            }
            real_decompressed_size => {
                decompressed_vec.truncate(real_decompressed_size as usize);
                return Some(decompressed_vec);
            }
        }
    }
}
