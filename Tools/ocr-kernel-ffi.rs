//! 开发基准的窄 C ABI；直接编译同一份内核，供同进程对照客户端 ORT DLL。
//! 不是产品插件接口；不拥有调用方内存，也不创建线程或运行库。
#[path = "../src/ppocr/kernels.rs"]
mod kernels;

use kernels::{Backend, Conv2d};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    slice,
};

fn valid_buffer(pointer: *const f32, count: usize) -> bool {
    !pointer.is_null()
        && (pointer as usize) % std::mem::align_of::<f32>() == 0
        && count <= isize::MAX as usize / 4
        && (pointer as usize).checked_add(count * 4).is_some()
}

fn overlaps(a: *const f32, an: usize, b: *const f32, bn: usize) -> bool {
    (a as usize) < b as usize + bn * 4 && (b as usize) < a as usize + an * 4
}

/// # Safety
/// 调用方必须为各指针提供对应长度的存活内存，params 是 14 个连续 usize。
/// 输入／权重／bias 在调用期间不可被修改，output 在调用期间不可被其它线程访问。
/// 非空 bias 长度须等于输出通道数。函数检查尺寸、对齐与输出不重叠，但不能验证外部指针的分配来源。
#[no_mangle]
pub unsafe extern "C" fn rustshot_ocr_probe_conv(
    params: *const usize,
    params_len: usize,
    input: *const f32,
    input_len: usize,
    weights: *const f32,
    weights_len: usize,
    bias: *const f32,
    bias_len: usize,
    output: *mut f32,
    output_len: usize,
) -> i32 {
    if params_len != 14
        || params.is_null()
        || (params as usize) % std::mem::align_of::<usize>() != 0
    {
        return -1;
    }
    // SAFETY: 外部契约提供 14 个存活 usize；所有 float 指针暂不解引用。
    let p = unsafe { slice::from_raw_parts(params, 14) };
    let conv = Conv2d {
        batch: p[0],
        in_channels: p[1],
        height: p[2],
        width: p[3],
        out_channels: p[4],
        kernel_h: p[5],
        kernel_w: p[6],
        stride_h: p[7],
        stride_w: p[8],
        pad_top: p[9],
        pad_left: p[10],
        pad_bottom: p[11],
        pad_right: p[12],
        groups: p[13],
    };
    if conv.input_len().ok() != Some(input_len)
        || conv.weights_len().ok() != Some(weights_len)
        || conv.output_len().ok() != Some(output_len)
        || (bias_len != 0 && bias_len != conv.out_channels)
        || !valid_buffer(input, input_len)
        || !valid_buffer(weights, weights_len)
        || !valid_buffer(output, output_len)
        || (bias_len != 0 && !valid_buffer(bias, bias_len))
    {
        return -1;
    }
    if overlaps(output, output_len, input, input_len)
        || overlaps(output, output_len, weights, weights_len)
        || (bias_len != 0 && overlaps(output, output_len, bias, bias_len))
    {
        return -1;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: 外部契约提供存活内存；上面已检查长度、地址范围、对齐和 output 排他范围。
        unsafe {
            conv.run(
                slice::from_raw_parts(input, input_len),
                slice::from_raw_parts(weights, weights_len),
                if bias_len == 0 {
                    None
                } else {
                    Some(slice::from_raw_parts(bias, bias_len))
                },
                slice::from_raw_parts_mut(output, output_len),
                Backend::Auto,
            )
        }
    }));
    match result {
        Ok(Ok(Backend::Avx2)) => 1,
        Ok(Ok(Backend::Scalar)) => 0,
        Ok(_) => -2,
        Err(_) => -3,
    }
}
