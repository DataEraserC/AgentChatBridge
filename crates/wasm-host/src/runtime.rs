//! wasmi 宿主运行时。
//!
//! 模块 ABI（固定、最小）：
//! - `memory`：线性内存；
//! - `acb_alloc(len: i32) -> i32`：给宿主写请求 JSON 的缓冲区；
//! - `acb_decide(ptr: i32, len: i32) -> i32`：读请求、返回结果缓冲区指针；
//! - `acb_result_len() -> i32`：结果字节数。
//!
//! 模块只负责决策，不做 I/O；所有 I/O 都在宿主进程里。

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use wasmi::{
    Config, Engine, ExternType, Instance, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc,
};

pub const PAGE_SIZE: usize = 64 * 1024;

struct HostState {
    limits: StoreLimits,
}

/// 模块返回的决策形状。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DecisionOutput {
    pub decision: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 宿主认可的一次决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub reason: Option<String>,
}

pub struct WasmHost {
    store: Store<HostState>,
    alloc: TypedFunc<i32, i32>,
    decide: TypedFunc<(i32, i32), i32>,
    result_len: TypedFunc<(), i32>,
    memory: Memory,
    fuel: u64,
}

impl WasmHost {
    /// 加载并实例化模块。任何一步失败都返回可读错误，调用方按硬失败处理。
    pub fn load(path: &Path, max_memory_bytes: usize, fuel: u64) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, &bytes[..])
            .map_err(|error| format!("invalid wasm module {}: {error}", path.display()))?;

        if let Some(ExternType::Memory(memory_type)) = module.get_export("memory") {
            let minimum = memory_type.minimum() as usize * PAGE_SIZE;
            if minimum > max_memory_bytes {
                return Err(format!(
                    "memory limit exceeded: module needs {minimum} bytes, limit is {max_memory_bytes}"
                ));
            }
        }

        let limits = StoreLimitsBuilder::new()
            .memory_size(max_memory_bytes)
            .build();
        let mut store = Store::new(&engine, HostState { limits });
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(fuel)
            .map_err(|error| format!("cannot set fuel: {error}"))?;

        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|error| format!("cannot instantiate module: {error}"))?;
        let alloc = typed(&instance, &store, "acb_alloc")?;
        let decide = typed(&instance, &store, "acb_decide")?;
        let result_len = typed(&instance, &store, "acb_result_len")?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| "missing export: memory".to_owned())?;

        Ok(Self {
            store,
            alloc,
            decide,
            result_len,
            memory,
            fuel,
        })
    }

    /// 调一次模块决策。trap / 越界 / 超 fuel 都返回 Err；调用方应把宿主进程
    /// 以非 0 退出，让 core 侧的 provider 立即 down。
    pub fn decide(&mut self, request: &Value) -> Result<Decision, String> {
        let input = serde_json::to_vec(request)
            .map_err(|error| format!("cannot serialize request: {error}"))?;
        self.store
            .set_fuel(self.fuel)
            .map_err(|error| format!("cannot reset fuel: {error}"))?;

        let ptr = self
            .alloc
            .call(&mut self.store, input.len() as i32)
            .map_err(|error| format!("acb_alloc trapped: {error}"))?;
        if ptr < 0 {
            return Err("acb_alloc refused the request".to_owned());
        }
        self.memory
            .write(&mut self.store, ptr as usize, &input)
            .map_err(|error| format!("cannot write request into wasm memory: {error}"))?;

        let out_ptr = self
            .decide
            .call(&mut self.store, (ptr, input.len() as i32))
            .map_err(|error| format!("acb_decide trapped: {error}"))?;
        if out_ptr < 0 {
            return Err("acb_decide returned a negative pointer".to_owned());
        }
        let len = self
            .result_len
            .call(&mut self.store, ())
            .map_err(|error| format!("acb_result_len trapped: {error}"))?;
        if len <= 0 || len as usize > 1024 {
            return Err(format!("invalid result length: {len}"));
        }
        let mut output = vec![0_u8; len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut output)
            .map_err(|error| format!("cannot read decision from wasm memory: {error}"))?;
        let parsed: DecisionOutput = serde_json::from_slice(&output)
            .map_err(|error| format!("module returned invalid decision JSON: {error}"))?;
        let allow = match parsed.decision.as_str() {
            "allow" => true,
            "deny" => false,
            other => return Err(format!("unknown access decision `{other}`")),
        };
        Ok(Decision {
            allow,
            reason: parsed.reason,
        })
    }
}

fn typed<Params, Results>(
    instance: &Instance,
    store: &Store<HostState>,
    name: &str,
) -> Result<TypedFunc<Params, Results>, String>
where
    Params: wasmi::WasmParams,
    Results: wasmi::WasmResults,
{
    let function = instance
        .get_func(store, name)
        .ok_or_else(|| format!("missing export: {name}"))?;
    function
        .typed::<Params, Results>(store)
        .map_err(|error| format!("export {name} has wrong signature: {error}"))
}
