use crate::ptx;
use crate::translate;
use rspirv::{
    binary::{Assemble, Disassemble},
    dr::{Block, Function, Instruction, Loader, Operand},
};
use spirv_headers::Word;
use spirv_tools_sys::{
    spv_binary, spv_endianness_t, spv_parsed_instruction_t, spv_result_t, spv_target_env,
};
use std::error;
use std::ffi::{c_void, CStr, CString};
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::mem;
use std::slice;
use std::{borrow::Cow, collections::HashMap, env, fs, path::PathBuf, ptr, str};
use std::{cmp, collections::hash_map::Entry};

macro_rules! test_ptx {
    ($fn_name:ident, $input:expr, $output:expr) => {
        paste::item! {
            #[test]
            fn [<$fn_name _ptx>]() -> Result<(), Box<dyn std::error::Error>> {
                let ptx = include_str!(concat!(stringify!($fn_name), ".ptx"));
                let input = $input;
                let mut output = $output;
                test_ptx_assert(stringify!($fn_name), ptx, &input, &mut output)
            }
        }

        paste::item! {
            #[test]
            fn [<$fn_name _spvtxt>]() -> Result<(), Box<dyn std::error::Error>> {
                let ptx_txt = include_str!(concat!(stringify!($fn_name), ".ptx"));
                let spirv_file_name = concat!(stringify!($fn_name), ".spvtxt");
                let spirv_txt = include_bytes!(concat!(stringify!($fn_name), ".spvtxt"));
                test_spvtxt_assert(ptx_txt, spirv_txt, spirv_file_name)
            }
        }
    };
}

test_ptx!(ld_st, [1u64], [1u64]);
test_ptx!(ld_st_implicit, [0.5f32], [0.5f32]);
test_ptx!(mov, [1u64], [1u64]);
test_ptx!(mul_lo, [1u64], [2u64]);
test_ptx!(mul_hi, [u64::max_value()], [1u64]);
test_ptx!(add, [1u64], [2u64]);
test_ptx!(setp, [10u64, 11u64], [1u64, 0u64]);
test_ptx!(setp_gt, [f32::NAN, 1f32], [1f32]);
test_ptx!(setp_leu, [1f32, f32::NAN], [1f32]);
test_ptx!(bra, [10u64], [11u64]);
test_ptx!(not, [0u64], [u64::max_value()]);
test_ptx!(shl, [11u64], [44u64]);
test_ptx!(shl_link_hack, [11u64], [44u64]);
test_ptx!(cvt_sat_s_u, [-1i32], [0i32]);
test_ptx!(cvta, [3.0f32], [3.0f32]);
test_ptx!(block, [1u64], [2u64]);
test_ptx!(local_align, [1u64], [1u64]);
test_ptx!(call, [1u64], [2u64]);
test_ptx!(vector, [1u32, 2u32], [3u32, 3u32]);
test_ptx!(ld_st_offset, [1u32, 2u32], [2u32, 1u32]);
test_ptx!(ntid, [3u32], [4u32]);
test_ptx!(reg_local, [12u64], [13u64]);
test_ptx!(mov_address, [0xDEADu64], [0u64]);
test_ptx!(b64tof64, [111u64], [111u64]);
test_ptx!(implicit_param, [34u32], [34u32]);
test_ptx!(pred_not, [10u64, 11u64], [2u64, 0u64]);
test_ptx!(mad_s32, [2i32, 3i32, 4i32], [10i32, 10i32, 10i32]);
test_ptx!(
    mul_wide,
    [0x01_00_00_00__01_00_00_00i64],
    [0x1_00_00_00_00_00_00i64]
);
test_ptx!(vector_extract, [1u8, 2u8, 3u8, 4u8], [3u8, 4u8, 1u8, 2u8]);
test_ptx!(shr, [-2i32], [-1i32]);
test_ptx!(or, [1u64, 2u64], [3u64]);
test_ptx!(sub, [2u64], [1u64]);
test_ptx!(min, [555i32, 444i32], [444i32]);
test_ptx!(max, [555i32, 444i32], [555i32]);
test_ptx!(global_array, [0xDEADu32], [1u32]);
test_ptx!(extern_shared, [127u64], [127u64]);
test_ptx!(extern_shared_call, [121u64], [123u64]);
test_ptx!(rcp, [2f32], [0.5f32]);
// 0b1_00000000_10000000000000000000000u32 is a large denormal
// 0x3f000000 is 0.5
test_ptx!(
    mul_ftz,
    [0b1_00000000_10000000000000000000000u32, 0x3f000000u32],
    [0b1_00000000_00000000000000000000000u32]
);
test_ptx!(
    mul_non_ftz,
    [0b1_00000000_10000000000000000000000u32, 0x3f000000u32],
    [0b1_00000000_01000000000000000000000u32]
);
test_ptx!(constant_f32, [10f32], [5f32]);
test_ptx!(constant_negative, [-101i32], [101i32]);
test_ptx!(and, [6u32, 3u32], [2u32]);
test_ptx!(selp, [100u16, 200u16], [200u16]);
test_ptx!(selp_true, [100u16, 200u16], [100u16]);
test_ptx!(fma, [2f32, 3f32, 5f32], [11f32]);
test_ptx!(shared_variable, [513u64], [513u64]);
test_ptx!(shared_ptr_32, [513u64], [513u64]);
test_ptx!(atom_cas, [91u32, 91u32], [91u32, 100u32]);
test_ptx!(atom_inc, [100u32], [100u32, 101u32, 0u32]);
test_ptx!(atom_add, [2u32, 4u32], [2u32, 6u32]);
test_ptx!(div_approx, [1f32, 2f32], [0.5f32]);
test_ptx!(sqrt, [0.25f32], [0.5f32]);
test_ptx!(rsqrt, [0.25f64], [2f64]);
test_ptx!(neg, [181i32], [-181i32]);
test_ptx!(sin, [std::f32::consts::PI / 2f32], [1f32]);
test_ptx!(cos, [std::f32::consts::PI], [-1f32]);
test_ptx!(lg2, [512f32], [9f32]);
test_ptx!(ex2, [10f32], [1024f32]);
test_ptx!(cvt_rni, [9.5f32, 10.5f32], [10f32, 10f32]);
test_ptx!(cvt_rzi, [-13.8f32, 12.9f32], [-13f32, 12f32]);
test_ptx!(cvt_s32_f32, [-13.8f32, 12.9f32], [-13i32, 13i32]);
test_ptx!(clz, [0b00000101_00101101_00010011_10101011u32], [5u32]);
test_ptx!(popc, [0b10111100_10010010_01001001_10001010u32], [14u32]);
test_ptx!(
    brev,
    [0b11000111_01011100_10101110_11111011u32],
    [0b11011111_01110101_00111010_11100011u32]
);
test_ptx!(
    xor,
    [
        0b01010010_00011010_01000000_00001101u32,
        0b11100110_10011011_00001100_00100011u32
    ],
    [0b10110100100000010100110000101110u32]
);
test_ptx!(rem, [21692i32, 13i32], [8i32]);
test_ptx!(
    bfe,
    [0b11111000_11000001_00100010_10100000u32, 16u32, 8u32],
    [0b11000001u32]
);
test_ptx!(stateful_ld_st_simple, [121u64], [121u64]);
test_ptx!(stateful_ld_st_ntid, [123u64], [123u64]);
test_ptx!(stateful_ld_st_ntid_chain, [12651u64], [12651u64]);
test_ptx!(stateful_ld_st_ntid_sub, [96311u64], [96311u64]);
test_ptx!(shared_ptr_take_address, [97815231u64], [97815231u64]);
// For now, we just make sure that it builds and links
test_ptx!(assertfail, [716523871u64], [716523872u64]);
test_ptx!(cvt_s64_s32, [-1i32], [-1i64]);

struct DisplayError<T: Debug> {
    err: T,
}

impl<T: Debug> Display for DisplayError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.err, f)
    }
}

impl<T: Debug> Debug for DisplayError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.err, f)
    }
}

impl<T: Debug> error::Error for DisplayError<T> {}

fn test_ptx_assert<
    'a,
    Input: From<u8> + ze::SafeRepr + Debug + Copy + PartialEq,
    Output: From<u8> + ze::SafeRepr + Debug + Copy + PartialEq,
>(
    name: &str,
    ptx_text: &'a str,
    input: &[Input],
    output: &mut [Output],
) -> Result<(), Box<dyn error::Error + 'a>> {
    let mut errors = Vec::new();
    let ast = ptx::ModuleParser::new().parse(&mut errors, ptx_text)?;
    assert!(errors.len() == 0);
    let zluda_module = translate::to_spirv_module(ast)?;
    let name = CString::new(name)?;
    let result = run_spirv(name.as_c_str(), zluda_module, input, output)
        .map_err(|err| DisplayError { err })?;
    assert_eq!(result.as_slice(), output);
    Ok(())
}

fn run_spirv<
    Input: From<u8> + ze::SafeRepr + Copy + Debug,
    Output: From<u8> + ze::SafeRepr + Copy + Debug,
>(
    name: &CStr,
    module: translate::Module,
    input: &[Input],
    output: &mut [Output],
) -> ze::Result<Vec<Output>> {
    ze::init()?;
    let spirv = module.spirv.assemble();
    let byte_il = unsafe {
        slice::from_raw_parts::<u8>(
            spirv.as_ptr() as *const _,
            spirv.len() * mem::size_of::<u32>(),
        )
    };
    let use_shared_mem = module
        .kernel_info
        .get(name.to_str().unwrap())
        .map(|info| info.uses_shared_mem)
        .unwrap_or(false);
    let mut result = vec![0u8.into(); output.len()];
    {
        let mut drivers = ze::Driver::get()?;
        let drv = drivers.drain(0..1).next().unwrap();
        let mut ctx = ze::Context::new(&drv)?;
        let mut devices = drv.devices()?;
        let dev = devices.drain(0..1).next().unwrap();
        let queue = ze::CommandQueue::new(&mut ctx, &dev)?;
        let (module, maybe_log) = match module.should_link_ptx_impl {
            Some(ptx_impl) => ze::Module::build_link_spirv(
                &mut ctx,
                &dev,
                &[ptx_impl, byte_il],
                Some(module.build_options.as_c_str()),
            ),
            None => {
                let (module, log) = ze::Module::build_spirv_logged(
                    &mut ctx,
                    &dev,
                    byte_il,
                    Some(module.build_options.as_c_str()),
                );
                (module, Some(log))
            }
        };
        let module = match module {
            Ok(m) => m,
            Err(err) => {
                let raw_err_string = maybe_log
                    .map(|log| log.get_cstring())
                    .transpose()?
                    .unwrap_or(CString::default());
                let err_string = raw_err_string.to_string_lossy();
                panic!("{:?}\n{}", err, err_string);
            }
        };
        let mut kernel = ze::Kernel::new_resident(&module, name)?;
        kernel.set_indirect_access(
            ze::sys::ze_kernel_indirect_access_flags_t::ZE_KERNEL_INDIRECT_ACCESS_FLAG_DEVICE,
        )?;
        let mut inp_b = ze::DeviceBuffer::<Input>::new(&mut ctx, &dev, cmp::max(input.len(), 1))?;
        let mut out_b = ze::DeviceBuffer::<Output>::new(&mut ctx, &dev, cmp::max(output.len(), 1))?;
        let inp_b_ptr_mut: ze::BufferPtrMut<Input> = (&mut inp_b).into();
        let event_pool = ze::EventPool::new(&mut ctx, 3, Some(&[&dev]))?;
        let ev0 = ze::Event::new(&event_pool, 0)?;
        let ev1 = ze::Event::new(&event_pool, 1)?;
        let mut ev2 = ze::Event::new(&event_pool, 2)?;
        let mut cmd_list = ze::CommandList::new(&mut ctx, &dev)?;
        let out_b_ptr_mut: ze::BufferPtrMut<Output> = (&mut out_b).into();
        let mut init_evs = [ev0, ev1];
        cmd_list.append_memory_copy(inp_b_ptr_mut, input, Some(&mut init_evs[0]), &mut [])?;
        cmd_list.append_memory_fill(out_b_ptr_mut, 0, Some(&mut init_evs[1]), &mut [])?;
        kernel.set_group_size(1, 1, 1)?;
        kernel.set_arg_buffer(0, inp_b_ptr_mut)?;
        kernel.set_arg_buffer(1, out_b_ptr_mut)?;
        if use_shared_mem {
            unsafe { kernel.set_arg_raw(2, 128, ptr::null())? };
        }
        cmd_list.append_launch_kernel(&kernel, &[1, 1, 1], Some(&mut ev2), &mut init_evs)?;
        cmd_list.append_memory_copy(result.as_mut_slice(), out_b_ptr_mut, None, &mut [ev2])?;
        queue.execute(cmd_list)?;
    }
    Ok(result)
}

fn test_spvtxt_assert<'a>(
    ptx_txt: &'a str,
    spirv_txt: &'a [u8],
    spirv_file_name: &'a str,
) -> Result<(), Box<dyn error::Error + 'a>> {
    let mut errors = Vec::new();
    let ast = ptx::ModuleParser::new().parse(&mut errors, ptx_txt)?;
    assert!(errors.len() == 0);
    let spirv_module = translate::to_spirv_module(ast)?;
    let spv_context =
        unsafe { spirv_tools::spvContextCreate(spv_target_env::SPV_ENV_UNIVERSAL_1_3) };
    assert!(spv_context != ptr::null_mut());
    let mut spv_binary: spv_binary = ptr::null_mut();
    let result = unsafe {
        spirv_tools::spvTextToBinary(
            spv_context,
            spirv_txt.as_ptr() as *const _,
            spirv_txt.len(),
            &mut spv_binary,
            ptr::null_mut(),
        )
    };
    if result != spv_result_t::SPV_SUCCESS {
        panic!("{:?}\n{}", result, unsafe {
            str::from_utf8_unchecked(spirv_txt)
        });
    }
    let mut parsed_spirv = Vec::<u32>::new();
    let result = unsafe {
        spirv_tools::spvBinaryParse(
            spv_context,
            &mut parsed_spirv as *mut _ as *mut _,
            (*spv_binary).code,
            (*spv_binary).wordCount,
            Some(parse_header_cb),
            Some(parse_instruction_cb),
            ptr::null_mut(),
        )
    };
    assert!(result == spv_result_t::SPV_SUCCESS);
    let mut loader = Loader::new();
    rspirv::binary::parse_words(&parsed_spirv, &mut loader)?;
    let spvtxt_mod = loader.module();
    unsafe { spirv_tools::spvBinaryDestroy(spv_binary) };
    if !is_spirv_fns_equal(&spirv_module.spirv.functions, &spvtxt_mod.functions) {
        // We could simply use ptx_mod.disassemble, but SPIRV-Tools text formattinmg is so much nicer
        let spv_from_ptx_binary = spirv_module.spirv.assemble();
        let mut spv_text: spirv_tools::spv_text = ptr::null_mut();
        let result = unsafe {
            spirv_tools::spvBinaryToText(
                spv_context,
                spv_from_ptx_binary.as_ptr(),
                spv_from_ptx_binary.len(),
                (spirv_tools::spv_binary_to_text_options_t::SPV_BINARY_TO_TEXT_OPTION_INDENT | spirv_tools::spv_binary_to_text_options_t::SPV_BINARY_TO_TEXT_OPTION_NO_HEADER |  spirv_tools::spv_binary_to_text_options_t::SPV_BINARY_TO_TEXT_OPTION_FRIENDLY_NAMES).0,
                &mut spv_text as *mut _,
                ptr::null_mut()
            )
        };
        unsafe { spirv_tools::spvContextDestroy(spv_context) };
        let spirv_text = if result == spv_result_t::SPV_SUCCESS {
            let raw_text = unsafe {
                std::slice::from_raw_parts((*spv_text).str_ as *const u8, (*spv_text).length)
            };
            let spv_from_ptx_text = unsafe { str::from_utf8_unchecked(raw_text) };
            // TODO: stop leaking kernel text
            Cow::Borrowed(spv_from_ptx_text)
        } else {
            Cow::Owned(spirv_module.spirv.disassemble())
        };
        if let Ok(dump_path) = env::var("ZLUDA_TEST_SPIRV_DUMP_DIR") {
            let mut path = PathBuf::from(dump_path);
            if let Ok(()) = fs::create_dir_all(&path) {
                path.push(spirv_file_name);
                #[allow(unused_must_use)]
                {
                    fs::write(path, spirv_text.as_bytes());
                }
            }
        }
        panic!(spirv_text.to_string());
    }
    unsafe { spirv_tools::spvContextDestroy(spv_context) };
    Ok(())
}

struct EqMap<T>
where
    T: Eq + Copy + Hash,
{
    m1: HashMap<T, T>,
    m2: HashMap<T, T>,
}

impl<T: Copy + Eq + Hash> EqMap<T> {
    fn new() -> Self {
        EqMap {
            m1: HashMap::new(),
            m2: HashMap::new(),
        }
    }

    fn is_equal(&mut self, t1: T, t2: T) -> bool {
        match (self.m1.entry(t1), self.m2.entry(t2)) {
            (Entry::Occupied(entry1), Entry::Occupied(entry2)) => {
                *entry1.get() == t2 && *entry2.get() == t1
            }
            (Entry::Vacant(entry1), Entry::Vacant(entry2)) => {
                entry1.insert(t2);
                entry2.insert(t1);
                true
            }
            _ => false,
        }
    }
}

fn is_spirv_fns_equal(fns1: &[Function], fns2: &[Function]) -> bool {
    if fns1.len() != fns2.len() {
        return false;
    }
    for (fn1, fn2) in fns1.iter().zip(fns2.iter()) {
        if !is_spirv_fn_equal(fn1, fn2) {
            return false;
        }
    }
    true
}

fn is_spirv_fn_equal(fn1: &Function, fn2: &Function) -> bool {
    let mut map = EqMap::new();
    if !is_option_equal(&fn1.def, &fn2.def, &mut map, is_instr_equal) {
        return false;
    }
    if !is_option_equal(&fn1.end, &fn2.end, &mut map, is_instr_equal) {
        return false;
    }
    if fn1.parameters.len() != fn2.parameters.len() {
        return false;
    }
    for (inst1, inst2) in fn1.parameters.iter().zip(fn2.parameters.iter()) {
        if !is_instr_equal(inst1, inst2, &mut map) {
            return false;
        }
    }
    if fn1.blocks.len() != fn2.blocks.len() {
        return false;
    }
    for (b1, b2) in fn1.blocks.iter().zip(fn2.blocks.iter()) {
        if !is_block_equal(b1, b2, &mut map) {
            return false;
        }
    }
    true
}

fn is_block_equal(b1: &Block, b2: &Block, map: &mut EqMap<Word>) -> bool {
    if !is_option_equal(&b1.label, &b2.label, map, is_instr_equal) {
        return false;
    }
    if b1.instructions.len() != b2.instructions.len() {
        return false;
    }
    for (inst1, inst2) in b1.instructions.iter().zip(b2.instructions.iter()) {
        if !is_instr_equal(inst1, inst2, map) {
            return false;
        }
    }
    true
}

fn is_instr_equal(instr1: &Instruction, instr2: &Instruction, map: &mut EqMap<Word>) -> bool {
    if instr1.class.opcode != instr2.class.opcode {
        return false;
    }
    if !is_option_equal(&instr1.result_type, &instr2.result_type, map, is_word_equal) {
        return false;
    }
    if !is_option_equal(&instr1.result_id, &instr2.result_id, map, is_word_equal) {
        return false;
    }
    if instr1.operands.len() != instr2.operands.len() {
        return false;
    }
    for (o1, o2) in instr1.operands.iter().zip(instr2.operands.iter()) {
        match (o1, o2) {
            (Operand::IdMemorySemantics(w1), Operand::IdMemorySemantics(w2)) => {
                if !is_word_equal(w1, w2, map) {
                    return false;
                }
            }
            (Operand::IdScope(w1), Operand::IdScope(w2)) => {
                if !is_word_equal(w1, w2, map) {
                    return false;
                }
            }
            (Operand::IdRef(w1), Operand::IdRef(w2)) => {
                if !is_word_equal(w1, w2, map) {
                    return false;
                }
            }
            (o1, o2) => {
                if o1 != o2 {
                    return false;
                }
            }
        }
    }
    true
}

fn is_word_equal(t1: &Word, t2: &Word, map: &mut EqMap<Word>) -> bool {
    map.is_equal(*t1, *t2)
}

fn is_option_equal<T, F: FnOnce(&T, &T, &mut EqMap<Word>) -> bool>(
    o1: &Option<T>,
    o2: &Option<T>,
    map: &mut EqMap<Word>,
    f: F,
) -> bool {
    match (o1, o2) {
        (Some(t1), Some(t2)) => f(t1, t2, map),
        (None, None) => true,
        _ => panic!(),
    }
}

unsafe extern "C" fn parse_header_cb(
    user_data: *mut c_void,
    endian: spv_endianness_t,
    magic: u32,
    version: u32,
    generator: u32,
    id_bound: u32,
    reserved: u32,
) -> spv_result_t {
    if endian == spv_endianness_t::SPV_ENDIANNESS_BIG {
        return spv_result_t::SPV_UNSUPPORTED;
    }
    let result_vec: &mut Vec<u32> = std::mem::transmute(user_data);
    result_vec.push(magic);
    result_vec.push(version);
    result_vec.push(generator);
    result_vec.push(id_bound);
    result_vec.push(reserved);
    spv_result_t::SPV_SUCCESS
}

unsafe extern "C" fn parse_instruction_cb(
    user_data: *mut c_void,
    inst: *const spv_parsed_instruction_t,
) -> spv_result_t {
    let inst = &*inst;
    let result_vec: &mut Vec<u32> = std::mem::transmute(user_data);
    for i in 0..inst.num_words {
        result_vec.push(*(inst.words.add(i as usize)));
    }
    spv_result_t::SPV_SUCCESS
}
