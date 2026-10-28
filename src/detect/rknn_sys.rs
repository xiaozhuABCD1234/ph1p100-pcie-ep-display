//! RKNN Runtime 2.3.2 的最小 FFI 绑定（RK3588）。
//!
//! 不链接 librknnrt：运行时用 dlopen 找 librknnrt.so，这样 x86 开发机
//! 不需要 RKNN SDK 也能编译、跑纯算法单元测试。结构与函数签名对照
//! rknn-toolkit2 v2.3.2 的 rknpu2/runtime/Linux/librknn_api/include/rknn_api.h。
//!
//! 只覆盖本项目要用的同步接口：init / query / set_core_mask / inputs_set /
//! run / outputs_get / outputs_release / destroy。

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::mem::size_of;
use std::ptr;
use std::slice;
use std::sync::Arc;

pub const RKNN_QUERY_IN_OUT_NUM: c_int = 0;
pub const RKNN_QUERY_INPUT_ATTR: c_int = 1;
pub const RKNN_QUERY_OUTPUT_ATTR: c_int = 2;

pub const RKNN_TENSOR_UINT8: u32 = 3;
pub const RKNN_TENSOR_NCHW: u32 = 0;
pub const RKNN_TENSOR_NHWC: u32 = 1;

pub const RKNN_NPU_CORE_AUTO: u32 = 0;
pub const RKNN_NPU_CORE_0: u32 = 1;
pub const RKNN_NPU_CORE_1: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RknnIoNum {
    pub n_input: u32,
    pub n_output: u32,
}

#[repr(C)]
pub struct RknnInput {
    pub index: u32,
    pub buf: *mut c_void,
    pub size: u32,
    pub pass_through: u8,
    pub type_: u32,
    pub fmt: u32,
}

impl Default for RknnInput {
    fn default() -> Self {
        RknnInput {
            index: 0,
            buf: ptr::null_mut(),
            size: 0,
            pass_through: 0,
            type_: RKNN_TENSOR_UINT8,
            fmt: RKNN_TENSOR_NHWC,
        }
    }
}

#[repr(C)]
pub struct RknnOutput {
    pub want_float: u8,
    pub is_prealloc: u8,
    pub index: u32,
    pub buf: *mut c_void,
    pub size: u32,
}

impl Default for RknnOutput {
    fn default() -> Self {
        RknnOutput {
            want_float: 1,
            is_prealloc: 0,
            index: 0,
            buf: ptr::null_mut(),
            size: 0,
        }
    }
}

#[repr(C)]
pub struct RknnTensorAttr {
    pub index: u32,
    pub n_dims: u32,
    pub dims: [u32; 16],
    pub name: [c_char; 256],
    pub n_elems: u32,
    pub size: u32,
    pub fmt: u32,
    pub type_: u32,
    pub qnt_type: u32,
    pub fl: i8,
    pub zp: i32,
    pub scale: f32,
    pub w_stride: u32,
    pub size_with_stride: u32,
    pub pass_through: u8,
    pub h_stride: u32,
}

impl Default for RknnTensorAttr {
    fn default() -> Self {
        RknnTensorAttr {
            index: 0,
            n_dims: 0,
            dims: [0; 16],
            name: [0; 256],
            n_elems: 0,
            size: 0,
            fmt: 0,
            type_: 0,
            qnt_type: 0,
            fl: 0,
            zp: 0,
            scale: 0.0,
            w_stride: 0,
            size_with_stride: 0,
            pass_through: 0,
            h_stride: 0,
        }
    }
}

impl RknnTensorAttr {
    pub fn dims(&self) -> &[u32] {
        &self.dims[..(self.n_dims as usize).min(self.dims.len())]
    }
}

type FnInit = unsafe extern "C" fn(*mut u64, *mut c_void, u32, u32, *mut c_void) -> c_int;
type FnDestroy = unsafe extern "C" fn(u64) -> c_int;
type FnQuery = unsafe extern "C" fn(u64, c_int, *mut c_void, u32) -> c_int;
type FnSetCoreMask = unsafe extern "C" fn(u64, u32) -> c_int;
type FnInputsSet = unsafe extern "C" fn(u64, u32, *mut RknnInput) -> c_int;
type FnRun = unsafe extern "C" fn(u64, *mut c_void) -> c_int;
type FnOutputsGet = unsafe extern "C" fn(u64, u32, *mut RknnOutput, *mut c_void) -> c_int;
type FnOutputsRelease = unsafe extern "C" fn(u64, u32, *mut RknnOutput) -> c_int;

/// 一次 dlopen 的函数表。Arc 共享给所有 context。
pub struct RknnApi {
    handle: *mut c_void,
    init: FnInit,
    destroy: FnDestroy,
    query: FnQuery,
    set_core_mask: FnSetCoreMask,
    inputs_set: FnInputsSet,
    run: FnRun,
    outputs_get: FnOutputsGet,
    outputs_release: FnOutputsRelease,
}

// dlopen 句柄与函数指针在进程内只读；context 由 worker 单线程持有。
unsafe impl Send for RknnApi {}
unsafe impl Sync for RknnApi {}

impl RknnApi {
    /// 加载 librknnrt.so（优先走系统 loader 搜索路径）。
    pub fn load() -> Result<Arc<Self>, String> {
        let cname = CString::new("librknnrt.so").expect("静态字符串没有 NUL");
        let handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!(
                "dlopen(librknnrt.so) 失败：{}（确认 /usr/lib/librknnrt.so 存在且架构为 aarch64）",
                dlerror()
            ));
        }
        // 任一符号缺失都必须干净失败；先取完再构造，避免半初始化。
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                match unsafe { sym::<$ty>(handle, $name) } {
                    Ok(f) => f,
                    Err(e) => {
                        unsafe { libc::dlclose(handle) };
                        return Err(e);
                    }
                }
            }};
        }
        let init = sym!("rknn_init", FnInit);
        let destroy = sym!("rknn_destroy", FnDestroy);
        let query = sym!("rknn_query", FnQuery);
        let set_core_mask = sym!("rknn_set_core_mask", FnSetCoreMask);
        let inputs_set = sym!("rknn_inputs_set", FnInputsSet);
        let run = sym!("rknn_run", FnRun);
        let outputs_get = sym!("rknn_outputs_get", FnOutputsGet);
        let outputs_release = sym!("rknn_outputs_release", FnOutputsRelease);
        Ok(Arc::new(RknnApi {
            handle,
            init,
            destroy,
            query,
            set_core_mask,
            inputs_set,
            run,
            outputs_get,
            outputs_release,
        }))
    }
}

impl Drop for RknnApi {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { libc::dlclose(self.handle) };
        }
    }
}

unsafe fn sym<T: Copy>(handle: *mut c_void, name: &str) -> Result<T, String> {
    let cname = CString::new(name).map_err(|_| format!("符号名有 NUL：{name}"))?;
    let p = libc::dlsym(handle, cname.as_ptr());
    if p.is_null() {
        return Err(format!("librknnrt.so 缺少符号 {name}"));
    }
    Ok(std::mem::transmute_copy(&p))
}

fn dlerror() -> String {
    unsafe {
        let p = libc::dlerror();
        if p.is_null() {
            "未知 dlopen 错误".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// 一个 RKNN context。Drop 时 rknn_destroy。
pub struct RknnContext {
    api: Arc<RknnApi>,
    ctx: u64,
}

unsafe impl Send for RknnContext {}

impl RknnContext {
    pub fn init(api: Arc<RknnApi>, model: &[u8], core_mask: u32) -> Result<Self, String> {
        if model.is_empty() {
            return Err("RKNN 模型文件是空的".into());
        }
        let mut ctx: u64 = 0;
        let rc = unsafe {
            (api.init)(
                &mut ctx,
                model.as_ptr() as *mut c_void,
                model.len() as u32,
                0,
                ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(format!("rknn_init 失败（{rc}）：{}", err_text(rc)));
        }
        if core_mask != RKNN_NPU_CORE_AUTO {
            let rc = unsafe { (api.set_core_mask)(ctx, core_mask) };
            if rc != 0 {
                unsafe { (api.destroy)(ctx) };
                return Err(format!(
                    "rknn_set_core_mask(0x{core_mask:x}) 失败（{rc}）：{}",
                    err_text(rc)
                ));
            }
        }
        Ok(RknnContext { api, ctx })
    }

    pub fn io_num(&self) -> Result<RknnIoNum, String> {
        let mut n = RknnIoNum::default();
        let rc = unsafe {
            (self.api.query)(
                self.ctx,
                RKNN_QUERY_IN_OUT_NUM,
                &mut n as *mut _ as *mut c_void,
                size_of::<RknnIoNum>() as u32,
            )
        };
        if rc != 0 {
            return Err(format!("查询输入/输出数量失败（{rc}）：{}", err_text(rc)));
        }
        Ok(n)
    }

    pub fn input_attr(&self, index: u32) -> Result<RknnTensorAttr, String> {
        let mut a = RknnTensorAttr::default();
        a.index = index;
        let rc = unsafe {
            (self.api.query)(
                self.ctx,
                RKNN_QUERY_INPUT_ATTR,
                &mut a as *mut _ as *mut c_void,
                size_of::<RknnTensorAttr>() as u32,
            )
        };
        if rc != 0 {
            return Err(format!(
                "查询输入 {index} 属性失败（{rc}）：{}",
                err_text(rc)
            ));
        }
        Ok(a)
    }

    pub fn output_attr(&self, index: u32) -> Result<RknnTensorAttr, String> {
        let mut a = RknnTensorAttr::default();
        a.index = index;
        let rc = unsafe {
            (self.api.query)(
                self.ctx,
                RKNN_QUERY_OUTPUT_ATTR,
                &mut a as *mut _ as *mut c_void,
                size_of::<RknnTensorAttr>() as u32,
            )
        };
        if rc != 0 {
            return Err(format!(
                "查询输出 {index} 属性失败（{rc}）：{}",
                err_text(rc)
            ));
        }
        Ok(a)
    }

    /// 同步跑一次。输入必须是与 input_w x input_h 匹配的 uint8 NHWC RGB。
    /// 输出用 want_float=1 取回 f32，长度至少 out_elems。
    pub fn run(&self, input: &[u8], out: &mut Vec<f32>, out_elems: usize) -> Result<(), String> {
        let mut inp = RknnInput {
            index: 0,
            buf: input.as_ptr() as *mut c_void,
            size: input.len() as u32,
            pass_through: 0,
            type_: RKNN_TENSOR_UINT8,
            // mean/std 归一化只接受 NHWC 源布局；这里必须与 letterbox 输出一致。
            fmt: RKNN_TENSOR_NHWC,
        };
        let rc = unsafe { (self.api.inputs_set)(self.ctx, 1, &mut inp) };
        if rc != 0 {
            return Err(format!("rknn_inputs_set 失败（{rc}）：{}", err_text(rc)));
        }
        let rc = unsafe { (self.api.run)(self.ctx, ptr::null_mut()) };
        if rc != 0 {
            return Err(format!("rknn_run 失败（{rc}）：{}", err_text(rc)));
        }
        let mut o = RknnOutput::default();
        let rc = unsafe { (self.api.outputs_get)(self.ctx, 1, &mut o, ptr::null_mut()) };
        if rc != 0 {
            return Err(format!("rknn_outputs_get 失败（{rc}）：{}", err_text(rc)));
        }
        if o.buf.is_null() {
            let _ = unsafe { (self.api.outputs_release)(self.ctx, 1, &mut o) };
            return Err("rknn_outputs_get 返回空指针".into());
        }
        let n = (o.size as usize) / size_of::<f32>();
        let n = n.min(out_elems);
        out.clear();
        out.extend_from_slice(unsafe { slice::from_raw_parts(o.buf as *const f32, n) });
        let rc = unsafe { (self.api.outputs_release)(self.ctx, 1, &mut o) };
        if rc != 0 {
            return Err(format!(
                "rknn_outputs_release 失败（{rc}）：{}",
                err_text(rc)
            ));
        }
        if n < out_elems {
            return Err(format!(
                "输出只有 {n} 个 f32，期望 {out_elems}（模型契约不符）"
            ));
        }
        Ok(())
    }

    /// 双输出版本：split ONNX 的 boxes [1,4,N] 与 scores [1,80,N] 分开量化。
    /// 两路都用 want_float=1 取回 f32。
    pub fn run2(
        &self,
        input: &[u8],
        out0: &mut Vec<f32>,
        n0: usize,
        out1: &mut Vec<f32>,
        n1: usize,
    ) -> Result<(), String> {
        let mut inp = RknnInput {
            index: 0,
            buf: input.as_ptr() as *mut c_void,
            size: input.len() as u32,
            pass_through: 0,
            type_: RKNN_TENSOR_UINT8,
            fmt: RKNN_TENSOR_NHWC,
        };
        let rc = unsafe { (self.api.inputs_set)(self.ctx, 1, &mut inp) };
        if rc != 0 {
            return Err(format!("rknn_inputs_set 失败（{rc}）：{}", err_text(rc)));
        }
        let rc = unsafe { (self.api.run)(self.ctx, ptr::null_mut()) };
        if rc != 0 {
            return Err(format!("rknn_run 失败（{rc}）：{}", err_text(rc)));
        }
        let mut outs = [RknnOutput::default(), RknnOutput::default()];
        outs[0].index = 0;
        outs[1].index = 1;
        let rc = unsafe { (self.api.outputs_get)(self.ctx, 2, outs.as_mut_ptr(), ptr::null_mut()) };
        if rc != 0 {
            return Err(format!(
                "rknn_outputs_get(2) 失败（{rc}）：{}",
                err_text(rc)
            ));
        }
        let copy = |o: &RknnOutput, dst: &mut Vec<f32>, want: usize| -> usize {
            if o.buf.is_null() {
                return 0;
            }
            let n = (o.size as usize / size_of::<f32>()).min(want);
            dst.clear();
            dst.extend_from_slice(unsafe { slice::from_raw_parts(o.buf as *const f32, n) });
            n
        };
        let got0 = copy(&outs[0], out0, n0);
        let got1 = copy(&outs[1], out1, n1);
        let rc = unsafe { (self.api.outputs_release)(self.ctx, 2, outs.as_mut_ptr()) };
        if rc != 0 {
            return Err(format!(
                "rknn_outputs_release(2) 失败（{rc}）：{}",
                err_text(rc)
            ));
        }
        if got0 < n0 || got1 < n1 {
            return Err(format!(
                "双输出长度不符（boxes {got0}/{n0}，scores {got1}/{n1}）"
            ));
        }
        Ok(())
    }
}

impl Drop for RknnContext {
    fn drop(&mut self) {
        unsafe { (self.api.destroy)(self.ctx) };
    }
}

pub fn err_text(rc: c_int) -> &'static str {
    match rc {
        0 => "成功",
        -1 => "RKNN_ERR_FAIL",
        -2 => "RKNN_ERR_TIMEOUT",
        -3 => "RKNN_ERR_DEVICE_UNAVAILABLE（检查 rknpu 模块与 renderD129）",
        -4 => "RKNN_ERR_MALLOC_FAIL",
        -5 => "RKNN_ERR_PARAM_INVALID",
        -6 => "RKNN_ERR_MODEL_INVALID（模型损坏或版本不兼容）",
        -7 => "RKNN_ERR_CTX_INVALID",
        -8 => "RKNN_ERR_INPUT_INVALID",
        -9 => "RKNN_ERR_OUTPUT_INVALID",
        -10 => "RKNN_ERR_DEVICE_UNMATCH（runtime 与驱动版本不匹配）",
        -11 => "RKNN_ERR_INCOMPATILE_PRE_COMPILE_MODEL",
        -12 => "RKNN_ERR_INCOMPATILE_OPTIMIZATION_LEVEL_VERSION",
        -13 => "RKNN_ERR_TARGET_PLATFORM_UNMATCH",
        _ => "未知 RKNN 错误",
    }
}
